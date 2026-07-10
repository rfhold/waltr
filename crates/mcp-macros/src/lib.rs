//! Procedural macros for building typed MCP servers.

use proc_macro::TokenStream;
use proc_macro2::TokenStream as TokenStream2;
use quote::quote;
use syn::{
    Attribute, Expr, ImplItem, ImplItemFn, ItemImpl, Meta, Path, Token, parse::Parser as _,
    parse_macro_input, punctuated::Punctuated,
};

/// Generate an `mcp::server::ServerHandler` implementation from an inherent impl block.
///
/// Supported method helper attributes are `tool`, `prompt`, `resource`,
/// `resource_template`, `complete`, `log_level`, `notification`, `instructions`, and
/// `token_validator`.
#[proc_macro_attribute]
pub fn mcp_server(args: TokenStream, input: TokenStream) -> TokenStream {
    let args = parse_macro_input!(args as ServerArgs);
    let input = parse_macro_input!(input as ItemImpl);
    expand_mcp_server(args, input)
        .unwrap_or_else(syn::Error::into_compile_error)
        .into()
}

fn expand_mcp_server(args: ServerArgs, mut input: ItemImpl) -> syn::Result<TokenStream2> {
    let self_ty = input.self_ty.clone();
    let generics = input.generics.clone();
    let (impl_generics, _ty_generics, where_clause) = generics.split_for_impl();

    let mut tools = Vec::new();
    let mut prompts = Vec::new();
    let mut resources = Vec::new();
    let mut resource_templates = Vec::new();
    let mut completion = None;
    let mut log_level = None;
    let mut notifications = Vec::new();
    let mut instructions = None;
    let mut token_validator = None;

    for item in &mut input.items {
        let ImplItem::Fn(method) = item else {
            continue;
        };

        let mut retained_attrs = Vec::new();
        for attr in &method.attrs {
            match helper_attr_name(attr).as_deref() {
                Some("tool") => tools.push(RegisteredMethod {
                    method: method.clone(),
                    attr: MethodAttr::from_attr(attr, "tool")?,
                }),
                Some("prompt") => prompts.push(RegisteredMethod {
                    method: method.clone(),
                    attr: MethodAttr::from_attr(attr, "prompt")?,
                }),
                Some("resource") => resources.push(RegisteredMethod {
                    method: method.clone(),
                    attr: MethodAttr::from_attr(attr, "resource")?,
                }),
                Some("resource_template") => resource_templates.push(RegisteredMethod {
                    method: method.clone(),
                    attr: MethodAttr::from_attr(attr, "resource_template")?,
                }),
                Some("complete") => completion = Some(method.clone()),
                Some("log_level") => log_level = Some(method.clone()),
                Some("notification") => notifications.push(NotificationMethod {
                    method: method.clone(),
                    attr: NotificationAttr::from_attr(attr)?,
                }),
                Some("instructions") => instructions = Some(method.clone()),
                Some("token_validator") => token_validator = Some(method.clone()),
                _ => retained_attrs.push(attr.clone()),
            }
        }
        method.attrs = retained_attrs;
    }

    let handler_impl = handler_impl(
        &args,
        &self_ty,
        &impl_generics,
        where_clause,
        HandlerParts {
            tools: &tools,
            prompts: &prompts,
            resources: &resources,
            resource_templates: &resource_templates,
            completion: completion.as_ref(),
            log_level: log_level.as_ref(),
            notifications: &notifications,
            instructions: instructions.as_ref(),
        },
    );
    let helpers_impl = helpers_impl(
        &args,
        &self_ty,
        &impl_generics,
        where_clause,
        token_validator.as_ref(),
    );

    Ok(quote! {
        #input

        #handler_impl

        #helpers_impl
    })
}

fn handler_impl(
    args: &ServerArgs,
    self_ty: &syn::Type,
    impl_generics: &syn::ImplGenerics<'_>,
    where_clause: Option<&syn::WhereClause>,
    parts: HandlerParts<'_>,
) -> TokenStream2 {
    let server_info = server_info_impl(args);
    let capabilities = capabilities_impl(args, &parts);
    let instructions = instructions_impl(parts.instructions);
    let list_tools = list_tools_impl(parts.tools, args.tools_next_cursor.as_ref());
    let call_tool = call_tool_impl(parts.tools);
    let list_prompts = list_prompts_impl(parts.prompts, args.prompts_next_cursor.as_ref());
    let get_prompt = get_prompt_impl(parts.prompts);
    let list_resources = list_resources_impl(parts.resources, args.resources_next_cursor.as_ref());
    let list_resource_templates = list_resource_templates_impl(
        parts.resource_templates,
        args.resource_templates_next_cursor.as_ref(),
    );
    let read_resource = read_resource_impl(parts.resources, parts.resource_templates);
    let complete = completion_impl(parts.completion);
    let log_level = log_level_impl(parts.log_level);
    let notifications = notifications_impl(parts.notifications);

    quote! {
        impl #impl_generics ::mcp::server::ServerHandler for #self_ty #where_clause {
            #server_info

            #capabilities

            #instructions

            #list_tools

            #call_tool

            #list_prompts

            #get_prompt

            #list_resources

            #list_resource_templates

            #read_resource

            #complete

            #log_level

            #notifications
        }
    }
}

fn server_info_impl(args: &ServerArgs) -> TokenStream2 {
    let name = &args.name;
    let version = &args.version;
    let description = args.description.as_ref().map(|description| {
        quote! {
            let info = info.with_description(#description);
        }
    });

    quote! {
        fn server_info(&self) -> ::mcp::server::ServerInfo {
            let info = ::mcp::server::ServerInfo::new(#name, #version);
            #description
            info
        }
    }
}

fn capabilities_impl(args: &ServerArgs, parts: &HandlerParts<'_>) -> TokenStream2 {
    let tools = args.capabilities.tools || !parts.tools.is_empty();
    let prompts = args.capabilities.prompts || !parts.prompts.is_empty();
    let resources = args.capabilities.resources
        || !parts.resources.is_empty()
        || !parts.resource_templates.is_empty();
    let completions = args.capabilities.completions || parts.completion.is_some();
    let logging = args.capabilities.logging || parts.log_level.is_some();

    let tools = tools.then(|| quote! { let capabilities = capabilities.tools(); });
    let prompts = prompts.then(|| quote! { let capabilities = capabilities.prompts(); });
    let resources = resources.then(|| quote! { let capabilities = capabilities.resources(); });
    let completions =
        completions.then(|| quote! { let capabilities = capabilities.completions(); });
    let logging = logging.then(|| quote! { let capabilities = capabilities.logging(); });

    quote! {
        fn capabilities(&self) -> ::mcp::server::ServerCapabilities {
            let capabilities = ::mcp::server::ServerCapabilities::new();
            #tools
            #prompts
            #resources
            #completions
            #logging
            capabilities
        }
    }
}

fn instructions_impl(method: Option<&ImplItemFn>) -> TokenStream2 {
    let Some(method) = method else {
        return TokenStream2::new();
    };
    let ident = &method.sig.ident;
    let call = if has_receiver(method) {
        quote! { self.#ident() }
    } else {
        quote! { Self::#ident() }
    };
    quote! {
        fn instructions(&self) -> Option<String> {
            #call
        }
    }
}

fn list_tools_impl(tools: &[RegisteredMethod], next_cursor: Option<&Expr>) -> TokenStream2 {
    if tools.is_empty() {
        return TokenStream2::new();
    }
    let definitions = tools.iter().map(|tool| tool.attr.required("definition"));
    let next_cursor = next_cursor_expr(next_cursor);
    quote! {
        fn list_tools(
            &self,
            _cursor: Option<String>,
            _context: ::mcp::server::ServerContext,
        ) -> ::mcp::server::BoxFuture<::mcp::server::ServerResult<::mcp::McpToolList>> {
            Box::pin(async {
                Ok(::mcp::McpToolList {
                    tools: vec![#(#definitions),*],
                    next_cursor: #next_cursor,
                })
            })
        }
    }
}

fn call_tool_impl(tools: &[RegisteredMethod]) -> TokenStream2 {
    if tools.is_empty() {
        return TokenStream2::new();
    }
    let branches = tools.iter().map(|tool| {
        let name = tool.attr.required("name");
        let call = boxed_method_call(&tool.method, quote! { call, context });
        quote! {
            if call.name == #name {
                return #call;
            }
        }
    });

    quote! {
        fn call_tool(
            &self,
            call: ::mcp::McpToolCall,
            context: ::mcp::server::ServerContext,
        ) -> ::mcp::server::BoxFuture<::mcp::server::ServerResult<::mcp::McpToolResult>> {
            #(#branches)*
            Box::pin(async move {
                Err(::mcp::server::ServerError::invalid_params(format!(
                    "Unknown tool: {}",
                    call.name
                )))
            })
        }
    }
}

fn list_prompts_impl(prompts: &[RegisteredMethod], next_cursor: Option<&Expr>) -> TokenStream2 {
    if prompts.is_empty() {
        return TokenStream2::new();
    }
    let definitions = prompts
        .iter()
        .map(|prompt| prompt.attr.required("definition"));
    let next_cursor = next_cursor_expr(next_cursor);
    quote! {
        fn list_prompts(
            &self,
            _cursor: Option<String>,
            _context: ::mcp::server::ServerContext,
        ) -> ::mcp::server::BoxFuture<::mcp::server::ServerResult<::mcp::McpPromptList>> {
            Box::pin(async {
                Ok(::mcp::McpPromptList {
                    prompts: vec![#(#definitions),*],
                    next_cursor: #next_cursor,
                })
            })
        }
    }
}

fn get_prompt_impl(prompts: &[RegisteredMethod]) -> TokenStream2 {
    if prompts.is_empty() {
        return TokenStream2::new();
    }
    let branches = prompts.iter().map(|prompt| {
        let prompt_name = prompt.attr.required("name");
        let call = boxed_method_call(&prompt.method, quote! { arguments, context });
        quote! {
            if name == #prompt_name {
                return #call;
            }
        }
    });

    quote! {
        fn get_prompt(
            &self,
            name: String,
            arguments: Option<::mcp::server::JsonValue>,
            context: ::mcp::server::ServerContext,
        ) -> ::mcp::server::BoxFuture<::mcp::server::ServerResult<::mcp::McpPromptResult>> {
            #(#branches)*
            Box::pin(async move {
                Err(::mcp::server::ServerError::invalid_params(format!(
                    "Invalid prompt name: {name}"
                )))
            })
        }
    }
}

fn list_resources_impl(resources: &[RegisteredMethod], next_cursor: Option<&Expr>) -> TokenStream2 {
    if resources.is_empty() {
        return TokenStream2::new();
    }
    let definitions = resources
        .iter()
        .map(|resource| resource.attr.required("definition"));
    let next_cursor = next_cursor_expr(next_cursor);
    quote! {
        fn list_resources(
            &self,
            _cursor: Option<String>,
            _context: ::mcp::server::ServerContext,
        ) -> ::mcp::server::BoxFuture<::mcp::server::ServerResult<::mcp::McpResourceList>> {
            Box::pin(async {
                Ok(::mcp::McpResourceList {
                    resources: vec![#(#definitions),*],
                    next_cursor: #next_cursor,
                })
            })
        }
    }
}

fn list_resource_templates_impl(
    templates: &[RegisteredMethod],
    next_cursor: Option<&Expr>,
) -> TokenStream2 {
    if templates.is_empty() {
        return TokenStream2::new();
    }
    let definitions = templates
        .iter()
        .map(|template| template.attr.required("definition"));
    let next_cursor = next_cursor_expr(next_cursor);
    quote! {
        fn list_resource_templates(
            &self,
            _cursor: Option<String>,
            _context: ::mcp::server::ServerContext,
        ) -> ::mcp::server::BoxFuture<::mcp::server::ServerResult<::mcp::McpResourceTemplateList>> {
            Box::pin(async {
                Ok(::mcp::McpResourceTemplateList {
                    resource_templates: vec![#(#definitions),*],
                    next_cursor: #next_cursor,
                })
            })
        }
    }
}

fn read_resource_impl(
    resources: &[RegisteredMethod],
    templates: &[RegisteredMethod],
) -> TokenStream2 {
    if resources.is_empty() && templates.is_empty() {
        return TokenStream2::new();
    }
    let resource_branches = resources.iter().map(|resource| {
        let uri = resource.attr.required("uri");
        let call = boxed_method_call(&resource.method, quote! { uri, context });
        quote! {
            if uri == #uri {
                return #call;
            }
        }
    });
    let template_branches = templates.iter().map(|template| {
        let definition = template.attr.required("definition");
        let call = boxed_method_call(&template.method, quote! { uri, context });
        quote! {
            if (#definition).matches_uri(&uri) {
                return #call;
            }
        }
    });

    quote! {
        fn read_resource(
            &self,
            uri: String,
            context: ::mcp::server::ServerContext,
        ) -> ::mcp::server::BoxFuture<::mcp::server::ServerResult<::mcp::McpResourceResult>> {
            #(#resource_branches)*
            #(#template_branches)*
            Box::pin(async move { Err(::mcp::server::ServerError::resource_not_found(uri)) })
        }
    }
}

fn completion_impl(method: Option<&ImplItemFn>) -> TokenStream2 {
    let Some(method) = method else {
        return TokenStream2::new();
    };
    let call = boxed_method_call(method, quote! { request, context });
    quote! {
        fn complete(
            &self,
            request: ::mcp::McpCompletionRequest,
            context: ::mcp::server::ServerContext,
        ) -> ::mcp::server::BoxFuture<::mcp::server::ServerResult<::mcp::McpCompletionResult>> {
            #call
        }
    }
}

fn log_level_impl(method: Option<&ImplItemFn>) -> TokenStream2 {
    let Some(method) = method else {
        return TokenStream2::new();
    };
    let call = boxed_method_call(method, quote! { level, context });
    quote! {
        fn set_log_level(
            &self,
            level: String,
            context: ::mcp::server::ServerContext,
        ) -> ::mcp::server::BoxFuture<::mcp::server::ServerResult<()>> {
            #call
        }
    }
}

fn notifications_impl(notifications: &[NotificationMethod]) -> TokenStream2 {
    if notifications.is_empty() {
        return TokenStream2::new();
    }
    let method_branches = notifications.iter().filter_map(|notification| {
        let method = notification.attr.method.as_ref()?;
        let call = boxed_unit_method_call(&notification.method, quote! { notification, context });
        Some(quote! {
            if notification.method == #method {
                return #call;
            }
        })
    });
    let fallback = notifications
        .iter()
        .find(|notification| notification.attr.method.is_none())
        .map_or_else(
            || quote! { Box::pin(async {}) },
            |notification| {
                boxed_unit_method_call(&notification.method, quote! { notification, context })
            },
        );

    quote! {
        fn handle_notification(
            &self,
            notification: ::mcp::protocol::JsonRpcNotification,
            context: ::mcp::server::ServerContext,
        ) -> ::mcp::server::BoxFuture<()> {
            #(#method_branches)*
            #fallback
        }
    }
}

fn helpers_impl(
    args: &ServerArgs,
    self_ty: &syn::Type,
    impl_generics: &syn::ImplGenerics<'_>,
    where_clause: Option<&syn::WhereClause>,
    token_validator: Option<&ImplItemFn>,
) -> TokenStream2 {
    let router_helpers = router_helpers_impl();
    let auth_helpers = token_validator.map(|method| auth_helpers_impl(args, method));

    quote! {
        impl #impl_generics #self_ty #where_clause {
            #router_helpers

            #auth_helpers
        }
    }
}

fn router_helpers_impl() -> TokenStream2 {
    quote! {
        pub fn streamable_http_router(self) -> ::mcp::server::StreamableHttpRouter
        where
            Self: Sized + ::mcp::server::ServerHandler,
        {
            ::mcp::server::streamable_http_router(::std::sync::Arc::new(self))
        }
    }
}

fn auth_helpers_impl(args: &ServerArgs, method: &ImplItemFn) -> TokenStream2 {
    let metadata_scopes = args.auth.scopes.as_ref().map(|scopes| {
        quote! {
            let metadata = metadata.with_scopes(#scopes);
        }
    });
    let resource_name = args.auth.resource_name.as_ref().map(|resource_name| {
        quote! {
            let metadata = metadata.with_resource_name(#resource_name);
        }
    });
    let validator = token_validator_closure(method);
    let required_scopes = args.auth.required_scopes.as_ref().map(|required_scopes| {
        quote! {
            let authorization = authorization.with_required_scopes(#required_scopes);
        }
    });
    let realm = args.auth.realm.as_ref().map(|realm| {
        quote! {
            let authorization = authorization.with_realm(#realm);
        }
    });

    quote! {
        pub fn protected_resource_metadata(
            resource: impl Into<String>,
            authorization_servers: impl IntoIterator<Item = impl Into<String>>,
        ) -> ::mcp::McpProtectedResourceMetadata {
            let metadata = ::mcp::McpProtectedResourceMetadata::new(resource, authorization_servers);
            #metadata_scopes
            #resource_name
            metadata
        }

        pub fn streamable_http_options(
            &self,
            metadata: ::mcp::McpProtectedResourceMetadata,
        ) -> ::mcp::Result<::mcp::server::StreamableHttpOptions>
        where
            Self: Sized + Clone,
        {
            let authorization = ::mcp::server::StreamableHttpAuthorization::new(metadata, #validator)?;
            #required_scopes
            #realm
            Ok(::mcp::server::StreamableHttpOptions::default().with_authorization(authorization))
        }

        pub fn streamable_http_router_with_auth(
            self,
            metadata: ::mcp::McpProtectedResourceMetadata,
        ) -> ::mcp::Result<::mcp::server::StreamableHttpRouter>
        where
            Self: Sized + Clone + ::mcp::server::ServerHandler,
        {
            let options = self.streamable_http_options(metadata)?;
            Ok(::mcp::server::streamable_http_router_with_options(
                ::std::sync::Arc::new(self),
                options,
            ))
        }
    }
}

fn token_validator_closure(method: &ImplItemFn) -> TokenStream2 {
    let ident = &method.sig.ident;
    let await_token = method.sig.asyncness.map(|_| quote! { .await });
    if has_receiver(method) {
        quote! {
            {
                let server = self.clone();
                move |token, context| {
                    let server = server.clone();
                    Box::pin(async move {
                        server.#ident(token, context)#await_token
                    }) as ::mcp::server::BoxFuture<::mcp::McpTokenValidation>
                }
            }
        }
    } else {
        quote! {
            move |token, context| {
                Box::pin(async move {
                    Self::#ident(token, context)#await_token
                }) as ::mcp::server::BoxFuture<::mcp::McpTokenValidation>
            }
        }
    }
}

fn boxed_method_call(method: &ImplItemFn, args: TokenStream2) -> TokenStream2 {
    let ident = &method.sig.ident;
    let await_token = method.sig.asyncness.map(|_| quote! { .await });
    if has_receiver(method) {
        quote! {
            {
                let server = self.clone();
                Box::pin(async move { server.#ident(#args)#await_token })
            }
        }
    } else {
        quote! {
            Box::pin(async move { Self::#ident(#args)#await_token })
        }
    }
}

fn boxed_unit_method_call(method: &ImplItemFn, args: TokenStream2) -> TokenStream2 {
    let ident = &method.sig.ident;
    let await_token = method.sig.asyncness.map(|_| quote! { .await });
    if has_receiver(method) {
        quote! {
            {
                let server = self.clone();
                Box::pin(async move {
                    server.#ident(#args)#await_token;
                })
            }
        }
    } else {
        quote! {
            Box::pin(async move {
                Self::#ident(#args)#await_token;
            })
        }
    }
}

fn next_cursor_expr(next_cursor: Option<&Expr>) -> TokenStream2 {
    next_cursor.map_or_else(
        || quote! { None },
        |cursor| quote! { Some((#cursor).into()) },
    )
}

fn has_receiver(method: &ImplItemFn) -> bool {
    method.sig.receiver().is_some()
}

fn helper_attr_name(attr: &Attribute) -> Option<String> {
    attr.path()
        .segments
        .last()
        .map(|segment| segment.ident.to_string())
        .filter(|name| is_helper_attr(name))
}

fn is_helper_attr(name: &str) -> bool {
    matches!(
        name,
        "tool"
            | "prompt"
            | "resource"
            | "resource_template"
            | "complete"
            | "log_level"
            | "notification"
            | "instructions"
            | "token_validator"
    )
}

struct HandlerParts<'a> {
    tools: &'a [RegisteredMethod],
    prompts: &'a [RegisteredMethod],
    resources: &'a [RegisteredMethod],
    resource_templates: &'a [RegisteredMethod],
    completion: Option<&'a ImplItemFn>,
    log_level: Option<&'a ImplItemFn>,
    notifications: &'a [NotificationMethod],
    instructions: Option<&'a ImplItemFn>,
}

struct RegisteredMethod {
    method: ImplItemFn,
    attr: MethodAttr,
}

struct MethodAttr {
    name: Option<Expr>,
    uri: Option<Expr>,
    definition: Option<Expr>,
}

impl MethodAttr {
    fn from_attr(attr: &Attribute, kind: &str) -> syn::Result<Self> {
        let mut parsed = Self {
            name: None,
            uri: None,
            definition: None,
        };

        attr.parse_nested_meta(|meta| {
            if meta.path.is_ident("name") {
                parsed.name = Some(meta.value()?.parse()?);
            } else if meta.path.is_ident("uri") || meta.path.is_ident("uri_template") {
                parsed.uri = Some(meta.value()?.parse()?);
            } else if meta.path.is_ident("definition") {
                parsed.definition = Some(meta.value()?.parse()?);
            } else {
                return Err(meta.error("unsupported MCP method attribute argument"));
            }
            Ok(())
        })?;

        match kind {
            "tool" | "prompt" if parsed.name.is_none() => {
                return Err(syn::Error::new_spanned(attr, "missing `name = ...`"));
            }
            "resource" | "resource_template" if parsed.uri.is_none() => {
                return Err(syn::Error::new_spanned(
                    attr,
                    "missing `uri = ...` or `uri_template = ...`",
                ));
            }
            _ => {}
        }
        if parsed.definition.is_none() {
            return Err(syn::Error::new_spanned(attr, "missing `definition = ...`"));
        }
        Ok(parsed)
    }

    fn required(&self, key: &str) -> &Expr {
        match key {
            "name" => self.name.as_ref().expect("validated method name"),
            "uri" => self.uri.as_ref().expect("validated resource uri"),
            "definition" => self.definition.as_ref().expect("validated definition"),
            _ => unreachable!("unknown method attribute key"),
        }
    }
}

struct NotificationMethod {
    method: ImplItemFn,
    attr: NotificationAttr,
}

#[derive(Default)]
struct NotificationAttr {
    method: Option<Expr>,
}

impl NotificationAttr {
    fn from_attr(attr: &Attribute) -> syn::Result<Self> {
        if matches!(attr.meta, Meta::Path(_)) {
            return Ok(Self::default());
        }
        let mut parsed = Self::default();
        attr.parse_nested_meta(|meta| {
            if meta.path.is_ident("method") {
                parsed.method = Some(meta.value()?.parse()?);
                Ok(())
            } else {
                Err(meta.error("unsupported notification attribute argument"))
            }
        })?;
        Ok(parsed)
    }
}

struct ServerArgs {
    name: Expr,
    version: Expr,
    description: Option<Expr>,
    capabilities: ExplicitCapabilities,
    auth: AuthArgs,
    tools_next_cursor: Option<Expr>,
    prompts_next_cursor: Option<Expr>,
    resources_next_cursor: Option<Expr>,
    resource_templates_next_cursor: Option<Expr>,
}

impl syn::parse::Parse for ServerArgs {
    fn parse(input: syn::parse::ParseStream<'_>) -> syn::Result<Self> {
        let metas = Punctuated::<Meta, Token![,]>::parse_terminated(input)?;
        let mut args = Self {
            name: syn::parse_quote!("mcp-server"),
            version: syn::parse_quote!("0"),
            description: None,
            capabilities: ExplicitCapabilities::default(),
            auth: AuthArgs::default(),
            tools_next_cursor: None,
            prompts_next_cursor: None,
            resources_next_cursor: None,
            resource_templates_next_cursor: None,
        };

        for meta in metas {
            match meta {
                Meta::NameValue(name_value) if path_is(&name_value.path, "name") => {
                    args.name = name_value.value;
                }
                Meta::NameValue(name_value) if path_is(&name_value.path, "version") => {
                    args.version = name_value.value;
                }
                Meta::NameValue(name_value) if path_is(&name_value.path, "description") => {
                    args.description = Some(name_value.value);
                }
                Meta::NameValue(name_value) if path_is(&name_value.path, "tools_next_cursor") => {
                    args.tools_next_cursor = Some(name_value.value);
                }
                Meta::NameValue(name_value) if path_is(&name_value.path, "prompts_next_cursor") => {
                    args.prompts_next_cursor = Some(name_value.value);
                }
                Meta::NameValue(name_value)
                    if path_is(&name_value.path, "resources_next_cursor") =>
                {
                    args.resources_next_cursor = Some(name_value.value);
                }
                Meta::NameValue(name_value)
                    if path_is(&name_value.path, "resource_templates_next_cursor") =>
                {
                    args.resource_templates_next_cursor = Some(name_value.value);
                }
                Meta::List(list) if path_is(&list.path, "capabilities") => {
                    args.capabilities = ExplicitCapabilities::from_tokens(list.tokens)?;
                }
                Meta::List(list) if path_is(&list.path, "auth") => {
                    args.auth = AuthArgs::from_tokens(list.tokens)?;
                }
                other => {
                    return Err(syn::Error::new_spanned(
                        other,
                        "unsupported mcp_server argument",
                    ));
                }
            }
        }

        Ok(args)
    }
}

fn path_is(path: &Path, ident: &str) -> bool {
    path.is_ident(ident)
}

#[derive(Default)]
struct ExplicitCapabilities {
    tools: bool,
    prompts: bool,
    resources: bool,
    completions: bool,
    logging: bool,
}

impl ExplicitCapabilities {
    fn from_tokens(tokens: TokenStream2) -> syn::Result<Self> {
        let parser = Punctuated::<Path, Token![,]>::parse_terminated;
        let paths = parser.parse2(tokens)?;
        let mut capabilities = Self::default();
        for path in paths {
            if path.is_ident("tools") {
                capabilities.tools = true;
            } else if path.is_ident("prompts") {
                capabilities.prompts = true;
            } else if path.is_ident("resources") {
                capabilities.resources = true;
            } else if path.is_ident("completions") {
                capabilities.completions = true;
            } else if path.is_ident("logging") {
                capabilities.logging = true;
            } else {
                return Err(syn::Error::new_spanned(
                    path,
                    "unsupported explicit capability",
                ));
            }
        }
        Ok(capabilities)
    }
}

#[derive(Default)]
struct AuthArgs {
    scopes: Option<Expr>,
    required_scopes: Option<Expr>,
    realm: Option<Expr>,
    resource_name: Option<Expr>,
}

impl AuthArgs {
    fn from_tokens(tokens: TokenStream2) -> syn::Result<Self> {
        let parser = Punctuated::<Meta, Token![,]>::parse_terminated;
        let metas = parser.parse2(tokens)?;
        let mut args = Self::default();
        for meta in metas {
            match meta {
                Meta::NameValue(name_value) if path_is(&name_value.path, "scopes") => {
                    args.scopes = Some(name_value.value);
                }
                Meta::NameValue(name_value) if path_is(&name_value.path, "required_scopes") => {
                    args.required_scopes = Some(name_value.value);
                }
                Meta::NameValue(name_value) if path_is(&name_value.path, "realm") => {
                    args.realm = Some(name_value.value);
                }
                Meta::NameValue(name_value) if path_is(&name_value.path, "resource_name") => {
                    args.resource_name = Some(name_value.value);
                }
                other => {
                    return Err(syn::Error::new_spanned(other, "unsupported auth argument"));
                }
            }
        }
        Ok(args)
    }
}
