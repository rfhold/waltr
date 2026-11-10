#include "quickjs.h"
#include "budgets.h"
#include <math.h>
#include <stdint.h>
#include <stdlib.h>
#include <string.h>

#define EXPORT(n) __attribute__((export_name(n)))
#define SLOTS 32
#define CAP 65536

static JSRuntime *rt;
static JSContext *ctx;
static JSValue compiled, namespace, root, input, object_proto, array_proto;
static JSClassID object_class, array_class, handle_class, error_class;
static int failure, phase, max_value, max_depth, max_nodes, max_string, max_binary;
static int budget_events;
static int max_calls, max_slots, call_count, next_id;
static const char *output;
static size_t output_size;

typedef struct {
    int id, queued, handle, binary_length, callback_active;
    char operation[129];
    const char *arguments;
    size_t arguments_length;
    uint8_t *binary;
    JSValue resolve, reject, callback, verdict;
} Slot;
static Slot slots[SLOTS];

static int fail(int code) { if (!failure) failure = code; return -1; }
void waltr_budget_event(int event) {
    budget_events |= event;
    fail(event == WALTR_BUDGET_STACK ? 2 : 3);
}
static JSValue invalid(int code) {
    fail(code);
    return JS_ThrowTypeError(ctx, "invalid bridge value");
}
static Slot *find(int id) {
    for (int i = 0; i < max_slots; i++) if (slots[i].id == id) return &slots[i];
    return NULL;
}
static void clear_output(void) {
    if (output) JS_FreeCString(ctx, output);
    output = NULL;
    output_size = 0;
}
static void clear_slot(Slot *s) {
    if (s->arguments) JS_FreeCString(ctx, s->arguments);
    free(s->binary);
    JS_FreeValue(ctx, s->resolve);
    JS_FreeValue(ctx, s->reject);
    JS_FreeValue(ctx, s->callback);
    JS_FreeValue(ctx, s->verdict);
    memset(s, 0, sizeof(*s));
}
static void free_descriptor(JSPropertyDescriptor *d) {
    JS_FreeValue(ctx, d->value);
    JS_FreeValue(ctx, d->getter);
    JS_FreeValue(ctx, d->setter);
}
static int array_index(const char *text, size_t length, uint32_t *index) {
    if (!length || length > 10 || (length > 1 && text[0] == '0')) return -1;
    uint32_t value = 0;
    for (size_t i = 0; i < length; i++) {
        if (text[i] < '0' || text[i] > '9') return -1;
        uint32_t digit = (uint32_t)(text[i] - '0');
        if (value > (UINT32_MAX - digit) / 10) return -1;
        value = value * 10 + digit;
    }
    *index = value;
    return 0;
}

/* Clone data descriptors only, without invoking user code or prototype hooks. */
static JSValue clone_value(JSValueConst value, int depth, int *nodes, size_t *bytes, void **ancestors) {
    *bytes += 1;
    if (depth > max_depth || ++*nodes > max_nodes || *bytes > (size_t)max_value) return invalid(1);
    if (JS_IsNull(value) || JS_IsBool(value)) return JS_DupValue(ctx, value);
    if (JS_IsString(value)) {
        size_t length;
        const char *text = JS_ToCStringLen(ctx, &length, value);
        if (!text) return JS_EXCEPTION;
        JS_FreeCString(ctx, text);
        *bytes += length;
        if (length > (size_t)max_string || *bytes > (size_t)max_value) return invalid(1);
        return JS_DupValue(ctx, value);
    }
    if (JS_IsNumber(value)) {
        double number;
        if (JS_ToFloat64(ctx, &number, value)) return JS_EXCEPTION;
        if (!isfinite(number) || (trunc(number) == number && fabs(number) > 9007199254740991.0)) return invalid(1);
        return JS_DupValue(ctx, value);
    }
    if (!JS_IsObject(value) || JS_IsProxy(value)) return invalid(1);
    JSClassID class_id = JS_GetClassID(value);
    int array = class_id == array_class;
    if (!array && class_id != object_class) return invalid(1);
    JSValue proto = JS_GetPrototype(ctx, value);
    int good_proto = JS_IsNull(proto) || JS_VALUE_GET_PTR(proto) == JS_VALUE_GET_PTR(array ? array_proto : object_proto);
    JS_FreeValue(ctx, proto);
    if (!good_proto) return invalid(1);
    void *identity = JS_VALUE_GET_PTR(value);
    for (int i = 0; i < depth; i++) if (ancestors[i] == identity) return invalid(1);
    ancestors[depth] = identity;
    JSPropertyEnum *keys = NULL;
    uint32_t count = 0;
    if (JS_GetOwnPropertyNames(ctx, &keys, &count, value, JS_GPN_STRING_MASK | JS_GPN_SYMBOL_MASK)) return JS_EXCEPTION;
    if (count > (uint32_t)max_nodes) { JS_FreePropertyEnum(ctx, keys, count); return invalid(1); }
    JSValue result = array ? JS_NewArray(ctx) : JS_NewObjectProto(ctx, JS_NULL);
    uint32_t array_length = 0, elements = 0, next_index = 0;
    int have_length = 0;
    int bad = JS_IsException(result);
    if (array && !bad && JS_SetPrototype(ctx, result, JS_NULL) < 0) bad = 1;
    for (uint32_t i = 0; i < count && !bad; i++) {
        JSValue key = JS_AtomToValue(ctx, keys[i].atom);
        if (!JS_IsString(key)) { JS_FreeValue(ctx, key); bad = 1; break; }
        size_t length;
        const char *text = JS_ToCStringLen(ctx, &length, key);
        JS_FreeValue(ctx, key);
        if (!text) { bad = 1; break; }
        JSPropertyDescriptor d;
        int own = JS_GetOwnProperty(ctx, &d, value, keys[i].atom);
        if (own != 1) { JS_FreeCString(ctx, text); bad = 1; break; }
        if ((d.flags & JS_PROP_TMASK) == JS_PROP_GETSET || !JS_IsUndefined(d.getter) || !JS_IsUndefined(d.setter)) bad = 1;
        if (array && length == 6 && !memcmp(text, "length", 6)) {
            double number = 0;
            if (bad || have_length || !JS_IsNumber(d.value) ||
                JS_ToFloat64(ctx, &number, d.value) || !isfinite(number) ||
                number < 0 || number > max_nodes || trunc(number) != number) bad = 1;
            else { array_length = (uint32_t)number; have_length = 1; }
        } else {
            if (!array) { *bytes += length; ++*nodes; }
            if (length > (size_t)max_string || *bytes > (size_t)max_value || *nodes > max_nodes || !(d.flags & JS_PROP_ENUMERABLE)) bad = 1;
            if (array) {
                uint32_t index;
                /* QuickJS enumerates unique integer keys in ascending order. */
                if (array_index(text, length, &index) || index >= (uint32_t)max_nodes || index != next_index) bad = 1;
                else next_index++;
                elements++;
            }
            if (!bad) {
                JSValue child = clone_value(d.value, depth + 1, nodes, bytes, ancestors);
                if (JS_IsException(child)) bad = 1;
                else if (JS_DefinePropertyValue(ctx, result, keys[i].atom, child, JS_PROP_C_W_E) < 0) bad = 1;
            }
        }
        free_descriptor(&d);
        JS_FreeCString(ctx, text);
    }
    JS_FreePropertyEnum(ctx, keys, count);
    if (array && (!have_length || array_length != elements || next_index != array_length)) bad = 1;
    if (bad) { JS_FreeValue(ctx, result); return invalid(1); }
    return result;
}
static const char *encode(JSValueConst value, size_t *length) {
    int nodes = 0;
    size_t bytes = 0;
    void *ancestors[66] = {0};
    JSValue copy = clone_value(value, 0, &nodes, &bytes, ancestors);
    if (JS_IsException(copy)) return NULL;
    JSValue json = JS_JSONStringify(ctx, copy, JS_UNDEFINED, JS_UNDEFINED);
    JS_FreeValue(ctx, copy);
    if (JS_IsException(json)) return NULL;
    const char *text = JS_ToCStringLen(ctx, length, json);
    JS_FreeValue(ctx, json);
    if (text && *length > (size_t)max_value) { JS_FreeCString(ctx, text); fail(1); return NULL; }
    return text;
}
static JSValue parse_data(const char *data, int length) {
    JSValue parsed = JS_ParseJSON(ctx, data, length, "host-data");
    if (JS_IsException(parsed)) return parsed;
    int nodes = 0;
    size_t bytes = 0;
    void *ancestors[66] = {0};
    JSValue copy = clone_value(parsed, 0, &nodes, &bytes, ancestors);
    JS_FreeValue(ctx, parsed);
    return copy;
}
static void handle_finalizer(JSRuntime *runtime, JSValue value) {
    free(JS_GetOpaque(value, handle_class));
    (void)runtime;
}
static void error_finalizer(JSRuntime *runtime, JSValue value) {
    free(JS_GetOpaque(value, error_class));
    (void)runtime;
}
static JSValue bridge(JSContext *context, JSValueConst self, int argc, JSValueConst *argv) {
    (void)context;
    (void)self;
    if (++call_count > max_calls) return invalid(2);
    if (argc < 2 || argc > 5 || !JS_IsString(argv[0])) return invalid(1);
    Slot *s = NULL;
    for (int i = 0; i < max_slots; i++) if (!slots[i].id) { s = &slots[i]; break; }
    if (!s) return invalid(2);
    size_t length;
    const char *operation = JS_ToCStringLen(ctx, &length, argv[0]);
    if (!operation) return JS_EXCEPTION;
    int valid = length > 0 && length <= 128;
    for (size_t i = 0; i < length; i++) {
        char c = operation[i];
        if (!((c >= 'a' && c <= 'z') || (c >= 'A' && c <= 'Z') || (c >= '0' && c <= '9') || c == '.' || c == '_' || c == '-')) valid = 0;
    }
    if (valid) memcpy(s->operation, operation, length + 1);
    JS_FreeCString(ctx, operation);
    if (!valid) return invalid(1);
    s->arguments = encode(argv[1], &s->arguments_length);
    if (!s->arguments) return JS_EXCEPTION;
    s->resolve = s->reject = s->callback = s->verdict = JS_UNDEFINED;
    if (argc > 2 && !JS_IsNull(argv[2]) && !JS_IsUndefined(argv[2])) {
        if (!JS_IsFunction(ctx, argv[2]) || JS_IsProxy(argv[2])) { clear_slot(s); return invalid(1); }
        s->callback = JS_DupValue(ctx, argv[2]);
    }
    s->binary_length = -1;
    if (argc > 3 && !JS_IsNull(argv[3]) && !JS_IsUndefined(argv[3])) {
        if (!JS_IsArrayBuffer(argv[3])) { clear_slot(s); return invalid(1); }
        uint8_t *data = JS_GetArrayBuffer(ctx, &length, argv[3]);
        if ((!data && JS_HasException(ctx)) || length > (size_t)max_binary) { clear_slot(s); return invalid(1); }
        s->binary = malloc(length ? length : 1);
        if (!s->binary) { clear_slot(s); return invalid(3); }
        memcpy(s->binary, data, length);
        s->binary_length = (int)length;
    }
    if (argc > 4 && !JS_IsNull(argv[4]) && !JS_IsUndefined(argv[4])) {
        unsigned *handle = JS_GetOpaque(argv[4], handle_class);
        if (!handle) { clear_slot(s); return invalid(1); }
        s->handle = (int)*handle;
    }
    JSValue funcs[2];
    JSValue promise = JS_NewPromiseCapability(ctx, funcs);
    if (JS_IsException(promise)) { clear_slot(s); return promise; }
    s->resolve = funcs[0]; s->reject = funcs[1];
    s->id = ++next_id; s->queued = 1;
    return promise;
}

EXPORT("init") int init(int heap, int stack, int value, int depth, int nodes, int string, int binary, int calls, int inflight) {
    max_value = value; max_depth = depth; max_nodes = nodes; max_string = string;
    max_binary = binary; max_calls = calls; max_slots = inflight;
    if (inflight < 1 || inflight > SLOTS || value > CAP || depth > 64 || nodes > 8192) return -1;
    rt = JS_NewRuntime();
    if (!rt) return -1;
    JS_SetMemoryLimit(rt, heap); JS_SetMaxStackSize(rt, stack);
    ctx = JS_NewContext(rt);
    if (!ctx) return -1;
    compiled = namespace = root = input = JS_UNDEFINED;
    JSValue object = JS_NewObject(ctx), array = JS_NewArray(ctx);
    object_class = JS_GetClassID(object); array_class = JS_GetClassID(array);
    object_proto = JS_GetPrototype(ctx, object); array_proto = JS_GetPrototype(ctx, array);
    JS_FreeValue(ctx, object); JS_FreeValue(ctx, array);
    JS_NewClassID(rt, &handle_class);
    JSClassDef def = { .class_name = "HostHandle", .finalizer = handle_finalizer };
    if (JS_NewClass(rt, handle_class, &def)) return -1;
    JS_SetClassProto(ctx, handle_class, JS_NULL);
    JS_NewClassID(rt, &error_class);
    JSClassDef error_def = { .class_name = "HostError", .finalizer = error_finalizer };
    if (JS_NewClass(rt, error_class, &error_def)) return -1;
    JS_SetClassProto(ctx, error_class, JS_NULL);
    JSValue host = JS_NewObjectProto(ctx, JS_NULL);
    JSValue fn = JS_NewCFunction(ctx, bridge, "call", 2);
    JS_FreezeObject(ctx, fn);
    JS_DefinePropertyValueStr(ctx, host, "call", fn, JS_PROP_ENUMERABLE);
    JS_FreezeObject(ctx, host);
    JSValue global = JS_GetGlobalObject(ctx);
    JS_DefinePropertyValueStr(ctx, global, "host", host, JS_PROP_ENUMERABLE);
    JS_FreeValue(ctx, global);
    return 0;
}
EXPORT("compile") int compile_source(const char *source, int length) {
    if (phase || length < 0 || length > CAP) return -1;
    compiled = JS_Eval(ctx, source, length, "routine.mjs", JS_EVAL_TYPE_MODULE | JS_EVAL_FLAG_COMPILE_ONLY);
    if (JS_IsException(compiled) || JS_ResolveModule(ctx, compiled)) return -1;
    phase = 1;
    return 0;
}
EXPORT("start") int start(const char *data, int length) {
    if (phase != 1) return -1;
    input = parse_data(data, length);
    if (JS_IsException(input)) return -1;
    JSModuleDef *module = JS_VALUE_GET_PTR(compiled);
    root = JS_EvalFunction(ctx, compiled);
    compiled = JS_UNDEFINED;
    if (JS_IsException(root)) return -1;
    namespace = JS_GetModuleNamespace(ctx, module);
    if (JS_IsException(namespace)) return -1;
    phase = 2;
    return 0;
}
EXPORT("step") int step(void) {
    JSContext *job_ctx;
    return JS_ExecutePendingJob(rt, &job_ctx);
}
EXPORT("failure") int failure_code(void) { return failure; }
EXPORT("budget_events") int observed_budget_events(void) { return budget_events; }
static int rejection(JSValueConst promise) {
    if (JS_PromiseState(ctx, promise) != JS_PROMISE_REJECTED) return 0;
    JSValue value = JS_PromiseResult(ctx, promise);
    int *code = JS_GetOpaque(value, error_class);
    int result = code ? *code : 0;
    JS_FreeValue(ctx, value);
    return result;
}
EXPORT("rejection_code") int rejection_code(void) { return rejection(root); }
EXPORT("callback_error") int callback_error(int id) { Slot *s = find(id); return s ? rejection(s->verdict) : 0; }
EXPORT("status") int status(void) {
    if (failure) return -1;
    int state = JS_PromiseState(ctx, root);
    if (state == JS_PROMISE_PENDING) return 0;
    if (state != JS_PROMISE_FULFILLED) return -1;
    if (phase == 2) {
        JSValue fn = JS_GetPropertyStr(ctx, namespace, "default");
        JSValue next = JS_Call(ctx, fn, JS_UNDEFINED, 1, &input);
        JS_FreeValue(ctx, fn); JS_FreeValue(ctx, root); root = next; phase = 3;
        if (JS_IsException(root) || JS_PromiseState(ctx, root) < 0) return -1;
        return 0;
    }
    return 1;
}
EXPORT("result") int result(void) {
    clear_output();
    JSValue value = JS_PromiseResult(ctx, root);
    output = encode(value, &output_size);
    JS_FreeValue(ctx, value);
    return output ? 0 : -1;
}
EXPORT("event") int event(void) {
    for (int i = 0; i < max_slots; i++) if (slots[i].id && slots[i].queued) return slots[i].id;
    return 0;
}
EXPORT("event_op") const char *event_op(int id) { Slot *s = find(id); return s ? s->operation : NULL; }
EXPORT("event_args") const char *event_args(int id) { Slot *s = find(id); return s ? s->arguments : NULL; }
EXPORT("event_args_len") int event_args_len(int id) { Slot *s = find(id); return s ? (int)s->arguments_length : -1; }
EXPORT("event_handle") int event_handle(int id) { Slot *s = find(id); return s ? s->handle : -1; }
EXPORT("event_bytes") const void *event_bytes(int id) { Slot *s = find(id); return s ? s->binary : NULL; }
EXPORT("event_bytes_len") int event_bytes_len(int id) { Slot *s = find(id); return s ? s->binary_length : -2; }
EXPORT("event_callback") int event_callback(int id) { Slot *s = find(id); return s ? !JS_IsUndefined(s->callback) : -1; }
EXPORT("event_pop") int event_pop(int id) { Slot *s = find(id); if (!s || !s->queued) return -1; s->queued = 0; return 0; }
EXPORT("settle") int settle(int id, int type, const uint8_t *data, int length, int handle) {
    Slot *s = find(id);
    if (!s || s->queued || s->callback_active || length < 0 || length > CAP) return -1;
    JSValue value;
    if (type == 0) {
        int *code = malloc(sizeof(*code));
        if (!code) return -1;
        *code = handle;
        value = JS_NewObjectClass(ctx, error_class);
        if (JS_IsException(value)) { free(code); return -1; }
        JS_SetOpaque(value, code);
        if (JS_FreezeObject(ctx, value) < 0) { JS_FreeValue(ctx, value); return -1; }
    }
    else if (type == 1) value = parse_data((const char *)data, length);
    else if (type == 2) {
        value = JS_NewArrayBufferCopy(ctx, data, length);
        if (!JS_IsException(value)) {
            if (JS_SetPrototype(ctx, value, JS_NULL) < 0 ||
                JS_DefinePropertyValueStr(ctx, value, "byteLength", JS_NewInt32(ctx, length), 0) < 0) {
                JS_FreeValue(ctx, value); return -1;
            }
        }
    }
    else if (type == 3) {
        unsigned *owned = malloc(sizeof(*owned));
        if (!owned) return -1;
        *owned = (unsigned)handle;
        value = JS_NewObjectClass(ctx, handle_class);
        if (JS_IsException(value)) { free(owned); return -1; }
        JS_SetOpaque(value, owned);
        if (JS_FreezeObject(ctx, value) < 0) { JS_FreeValue(ctx, value); return -1; }
    } else return -1;
    if (JS_IsException(value)) return -1;
    JSValue called = JS_Call(ctx, type ? s->resolve : s->reject, JS_UNDEFINED, 1, &value);
    JS_FreeValue(ctx, value);
    int bad = JS_IsException(called);
    JS_FreeValue(ctx, called); clear_slot(s);
    return bad ? -1 : 0;
}
EXPORT("callback") int callback(int id, const char *data, int length) {
    Slot *s = find(id);
    if (!s || s->queued || s->callback_active || JS_IsUndefined(s->callback)) return -1;
    JSValue value = parse_data(data, length);
    if (JS_IsException(value)) return -1;
    s->verdict = JS_Call(ctx, s->callback, JS_UNDEFINED, 1, &value);
    JS_FreeValue(ctx, value);
    if (JS_IsException(s->verdict) || JS_PromiseState(ctx, s->verdict) < 0) return -1;
    s->callback_active = 1;
    return 0;
}
EXPORT("callback_status") int callback_status(int id) {
    Slot *s = find(id);
    if (!s || !s->callback_active) return -1;
    int state = JS_PromiseState(ctx, s->verdict);
    return state == JS_PROMISE_PENDING ? 0 : state == JS_PROMISE_FULFILLED ? 1 : -1;
}
EXPORT("callback_result") int callback_result(int id) {
    Slot *s = find(id);
    if (!s || !s->callback_active) return -1;
    clear_output();
    JSValue value = JS_PromiseResult(ctx, s->verdict);
    output = encode(value, &output_size);
    JS_FreeValue(ctx, value); JS_FreeValue(ctx, s->verdict);
    s->verdict = JS_UNDEFINED; s->callback_active = 0;
    return output ? 0 : -1;
}
EXPORT("output_ptr") const char *output_ptr(void) { return output; }
EXPORT("output_len") int output_len(void) { return (int)output_size; }
EXPORT("allocate") void *allocate(int size) { return size > 0 && size <= CAP + 1 ? malloc(size) : NULL; }
EXPORT("deallocate") void deallocate(void *ptr) { free(ptr); }
EXPORT("cleanup") int cleanup(void) {
    for (int i = 0; i < max_slots; i++) if (slots[i].id) clear_slot(&slots[i]);
    clear_output();
    JS_FreeValue(ctx, compiled); JS_FreeValue(ctx, namespace); JS_FreeValue(ctx, root);
    JS_FreeValue(ctx, input); JS_FreeValue(ctx, object_proto); JS_FreeValue(ctx, array_proto);
    JSValue exception = JS_GetException(ctx); JS_FreeValue(ctx, exception);
    JS_FreeContext(ctx); JS_FreeRuntime(rt); ctx = NULL; rt = NULL;
    return 0;
}
