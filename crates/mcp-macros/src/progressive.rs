use std::collections::BTreeSet;

use proc_macro2::TokenStream as TokenStream2;
use quote::quote;
use syn::{
    Attribute, Expr, FnArg, ImplItem, ImplItemFn, ItemImpl, Lit, LitStr, Meta, Path, Token, Type,
    parse::Parser as _, punctuated::Punctuated,
};

#[allow(clippy::redundant_pub_crate)]
pub(crate) fn expand_progressive_server(
    args: &ProgressiveArgs,
    mut input: ItemImpl,
) -> syn::Result<TokenStream2> {
    let self_ty = input.self_ty.clone();
    let generics = input.generics.clone();
    let (impl_generics, _ty_generics, where_clause) = generics.split_for_impl();

    let mut actions = Vec::new();
    let mut token_validator = None;

    for item in &mut input.items {
        let ImplItem::Fn(method) = item else {
            continue;
        };

        let mut retained_attrs = Vec::new();
        for attr in &method.attrs {
            match helper_attr_name(attr).as_deref() {
                Some("action") => actions.push(ProgressiveAction::from_method(method, attr)?),
                Some("token_validator") => token_validator = Some(method.clone()),
                _ => retained_attrs.push(attr.clone()),
            }
        }
        method.attrs = retained_attrs;
    }

    validate_progressive_parts(args, &actions)?;

    let handler_impl = handler_impl(
        args,
        &self_ty,
        &impl_generics,
        where_clause,
        ProgressiveParts { actions: &actions },
    );
    let helpers_impl = helpers_impl(
        args,
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
    args: &ProgressiveArgs,
    self_ty: &Type,
    impl_generics: &syn::ImplGenerics<'_>,
    where_clause: Option<&syn::WhereClause>,
    parts: ProgressiveParts<'_>,
) -> TokenStream2 {
    let ProgressiveParts { actions } = parts;
    let server_info = server_info_impl(args);
    let capabilities = capabilities_impl(args, actions);
    let catalog = super::skill_catalog_impl(args.skills.as_ref());
    let list_tools = list_tools_impl(args, actions);
    let call_tool = call_tool_impl(args, actions);
    let list_resources = list_resources_impl(actions);
    let list_resource_templates = list_resource_templates_impl(actions);
    let read_resource = read_resource_impl(actions);

    quote! {
        impl #impl_generics ::mcp::server::ServerHandler for #self_ty #where_clause {
            #server_info

            #capabilities

            #catalog

            #list_tools

            #call_tool

            #list_resources

            #list_resource_templates

            #read_resource
        }
    }
}

fn server_info_impl(args: &ProgressiveArgs) -> TokenStream2 {
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

fn capabilities_impl(args: &ProgressiveArgs, actions: &[ProgressiveAction]) -> TokenStream2 {
    let resources = actions
        .iter()
        .any(ProgressiveAction::exposes_resource)
        .then(|| quote! { let capabilities = capabilities.resources(); });
    let skills = args.skills.as_ref().map(|_| quote! { let capabilities = if self.skill_catalog().is_some_and(|catalog| !catalog.is_empty()) { capabilities.skills() } else { capabilities }; });
    quote! {
        fn capabilities(&self) -> ::mcp::server::ServerCapabilities {
            let capabilities = ::mcp::server::ServerCapabilities::new().tools();
            #resources
            #skills
            capabilities
        }
    }
}

fn list_tools_impl(args: &ProgressiveArgs, actions: &[ProgressiveAction]) -> TokenStream2 {
    let definitions =
        args.tools.iter().map(|tool| {
            let name = &tool.name;
            let description = &tool.description;
            let input_schema = input_schema_expr(tool, actions);
            tool.annotations.as_ref().map_or_else(
            || quote! { ::mcp::progressive::tool_definition(#name, #description, #input_schema) },
            |annotations| quote! {
                {
                    let mut definition =
                        ::mcp::progressive::tool_definition(#name, #description, #input_schema);
                    definition.annotations = (#annotations).into();
                    definition
                }
            },
        )
        });
    quote! {
        fn list_tools(
            &self,
            _cursor: Option<String>,
            _context: ::mcp::server::ServerContext,
        ) -> ::mcp::server::BoxFuture<::mcp::server::ServerResult<::mcp::McpToolList>> {
            Box::pin(async {
                Ok(::mcp::McpToolList {
                    tools: vec![#(#definitions),*],
                    next_cursor: None,
                })
            })
        }
    }
}

#[allow(clippy::too_many_lines)]
fn call_tool_impl(args: &ProgressiveArgs, actions: &[ProgressiveAction]) -> TokenStream2 {
    let tool_branches = args.tools.iter().map(|tool| {
        let tool_name = &tool.name;
        let tool_actions = actions
            .iter()
            .filter(|action| action.tool == tool.name.value())
            .collect::<Vec<_>>();
        let action_branches = tool_actions.iter().map(|action| {
            let name = action.name.value();
            let call = action_tool_call(action);
            quote! {
                #name => {
                    return #call;
                }
            }
        });
        quote! {
            if call.name == #tool_name {
                let request = match ::mcp::progressive::parse_arguments(#tool_name, call.arguments) {
                    Ok(request) => request,
                    Err(error) => return Box::pin(async move { Err(error) }),
                };
                match request.action.as_str() {
                    #(#action_branches)*
                    _ => {}
                }
                return Box::pin(async move {
                    Err(::mcp::progressive::unknown_action(&request.action))
                });
            }
        }
    });

    quote! {
        fn call_tool(
            &self,
            call: ::mcp::McpToolCall,
            context: ::mcp::server::ServerContext,
        ) -> ::mcp::server::BoxFuture<::mcp::server::ServerResult<::mcp::McpToolResult>> {
            #(#tool_branches)*
            Box::pin(async move {
                Err(::mcp::server::ServerError::invalid_params(format!(
                    "unknown progressive tool `{}`",
                    call.name
                )))
            })
        }
    }
}

fn list_resources_impl(actions: &[ProgressiveAction]) -> TokenStream2 {
    let resources = actions.iter().filter_map(|action| {
        let uri = action.attr.resource.as_ref()?;
        let name = &action.name;
        let description = &action.docs.description;
        Some(quote! { ::mcp::progressive::action_resource_definition(#uri, #name, #description) })
    });
    quote! {
        fn list_resources(
            &self,
            _cursor: Option<String>,
            _context: ::mcp::server::ServerContext,
        ) -> ::mcp::server::BoxFuture<::mcp::server::ServerResult<::mcp::McpResourceList>> {
            Box::pin(async {
                Ok(::mcp::McpResourceList {
                    resources: vec![#(#resources),*],
                    next_cursor: None,
                })
            })
        }
    }
}

fn list_resource_templates_impl(actions: &[ProgressiveAction]) -> TokenStream2 {
    let action_templates = actions.iter().filter_map(|action| {
        let uri_template = action.attr.resource_template.as_ref()?;
        let name = &action.name;
        let description = &action.docs.description;
        Some(quote! { ::mcp::progressive::action_resource_template_definition(#uri_template, #name, #description) })
    });
    quote! {
        fn list_resource_templates(
            &self,
            _cursor: Option<String>,
            _context: ::mcp::server::ServerContext,
        ) -> ::mcp::server::BoxFuture<::mcp::server::ServerResult<::mcp::McpResourceTemplateList>> {
            Box::pin(async {
                let templates = vec![#(#action_templates),*];
                Ok(::mcp::McpResourceTemplateList {
                    resource_templates: templates,
                    next_cursor: None,
                })
            })
        }
    }
}

fn read_resource_impl(actions: &[ProgressiveAction]) -> TokenStream2 {
    let resource_branches = actions.iter().filter_map(|action| {
        let uri = action.attr.resource.as_ref()?;
        let call = action_resource_call(action, &quote! { uri.clone() });
        Some(quote! {
            if uri == #uri {
                return #call;
            }
        })
    });
    let template_branches = actions.iter().filter_map(|action| {
        let uri_template = action.attr.resource_template.as_ref()?;
        let call = action_template_resource_call(action, uri_template, &quote! { uri.clone() });
        Some(quote! {
            if ::mcp::progressive::template_bindings(#uri_template, &uri).is_some() {
                return #call;
            }
        })
    });
    quote! {
        fn read_resource(
            &self,
            uri: String,
            context: ::mcp::server::ServerContext,
        ) -> ::mcp::server::BoxFuture<::mcp::server::ServerResult<::mcp::McpResourceResult>> {
            if let Some(result) = self.skill_catalog().and_then(|catalog| catalog.read(&uri)) {
                return Box::pin(async move { Ok(result) });
            }
            #(#resource_branches)*
            #(#template_branches)*
            Box::pin(async move { Err(::mcp::server::ServerError::resource_not_found(uri)) })
        }
    }
}

fn helpers_impl(
    args: &ProgressiveArgs,
    self_ty: &Type,
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
        #[doc = "Build a Streamable HTTP router for this progressive server."]
        pub fn streamable_http_router(self) -> ::mcp::server::StreamableHttpRouter
        where
            Self: Sized + ::mcp::server::ServerHandler,
        {
            ::mcp::server::streamable_http_router(::std::sync::Arc::new(self))
        }
    }
}

fn auth_helpers_impl(args: &ProgressiveArgs, method: &ImplItemFn) -> TokenStream2 {
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
        #[doc = "Build protected-resource metadata using this server's declared authorization settings."]
        pub fn protected_resource_metadata(
            resource: impl Into<String>,
            authorization_servers: impl IntoIterator<Item = impl Into<String>>,
        ) -> ::mcp::McpProtectedResourceMetadata {
            let metadata = ::mcp::McpProtectedResourceMetadata::new(resource, authorization_servers);
            #metadata_scopes
            #resource_name
            metadata
        }

        #[doc = "Build Streamable HTTP options with this server's token validator."]
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

        #[doc = "Build an authorized Streamable HTTP router for this server."]
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

fn input_schema_expr(tool: &ProgressiveToolArg, actions: &[ProgressiveAction]) -> TokenStream2 {
    let namespaces = tool.namespaces.iter().map(|namespace| {
        let name = &namespace.name;
        let description = &namespace.description;
        quote! { ::mcp::server::JsonValue::String(format!("{}: {}", #name, #description)) }
    });
    let action_names = actions
        .iter()
        .filter(|action| action.tool == tool.name.value())
        .map(|action| &action.name);
    let inputs = actions
        .iter()
        .filter(|action| action.tool == tool.name.value())
        .map(|action| {
            let name = &action.name;
            action.call.input_ty.as_ref().map_or_else(
                || {
                    quote! {
                        ::mcp::progressive::ActionInputSchema {
                            action: #name,
                            schema: None,
                        }
                    }
                },
                |input_ty| {
                    quote! {
                        ::mcp::progressive::ActionInputSchema {
                            action: #name,
                            schema: Some(::mcp::progressive::json_schema_for::<#input_ty>()),
                        }
                    }
                },
            )
        });
    quote! {
        {
        let mut schema = ::mcp::progressive::input_schema(
            &[#(#action_names),*],
            vec![#(#inputs),*],
        );
        schema["x-namespaces"] = ::mcp::server::JsonValue::Array(vec![#(#namespaces),*]);
        schema
        }
    }
}

fn action_tool_call(action: &ProgressiveAction) -> TokenStream2 {
    let invocation = action_invocation(action, &quote! { input }, &quote! { context });
    let input_prepare = action_input_prepare(action, &quote! { request.input });
    quote! {
        {
            #input_prepare
            let future = #invocation;
            Box::pin(async move {
                let output = future.await?;
                ::mcp::progressive::action_tool_result(output, request.filter.as_deref())
            }) as ::mcp::server::BoxFuture<::mcp::server::ServerResult<::mcp::McpToolResult>>
        }
    }
}

fn action_resource_call(action: &ProgressiveAction, uri: &TokenStream2) -> TokenStream2 {
    let invocation = action_invocation(action, &quote! { () }, &quote! { context });
    let input_prepare = quote! { let input = (); };
    quote! {
        {
            #input_prepare
            let future = #invocation;
            Box::pin(async move {
                let output = future.await?;
                ::mcp::progressive::action_resource_result(#uri, output)
            }) as ::mcp::server::BoxFuture<::mcp::server::ServerResult<::mcp::McpResourceResult>>
        }
    }
}

fn action_template_resource_call(
    action: &ProgressiveAction,
    uri_template: &LitStr,
    uri: &TokenStream2,
) -> TokenStream2 {
    let action_name = action.name.value();
    let invocation = action_invocation(action, &quote! { input }, &quote! { context });
    let input_prepare = action.call.input_ty.as_ref().map_or_else(
        || quote! { let input = (); },
        |input_ty| quote! {
            let Some(bindings) = ::mcp::progressive::template_bindings(#uri_template, &uri) else {
                return Box::pin(async move { Err(::mcp::server::ServerError::resource_not_found(uri)) });
            };
            let input = match ::mcp::progressive::deserialize_action_input::<#input_ty>(
                #action_name,
                Some(::mcp::server::JsonValue::Object(bindings)),
            ) {
                Ok(input) => input,
                Err(error) => return Box::pin(async move { Err(error) }),
            };
        },
    );
    quote! {
        {
            #input_prepare
            let future = #invocation;
            Box::pin(async move {
                let output = future.await?;
                ::mcp::progressive::action_resource_result(#uri, output)
            }) as ::mcp::server::BoxFuture<::mcp::server::ServerResult<::mcp::McpResourceResult>>
        }
    }
}

fn action_input_prepare(action: &ProgressiveAction, input: &TokenStream2) -> TokenStream2 {
    let action_name = action.name.value();
    action.call.input_ty.as_ref().map_or_else(
        || quote! {
            if let Err(error) = ::mcp::progressive::ensure_no_action_input(#action_name, #input) {
                return Box::pin(async move { Err(error) });
            }
            let input = ();
        },
        |input_ty| quote! {
            let input = match ::mcp::progressive::deserialize_action_input::<#input_ty>(
                #action_name,
                #input,
            ) {
                Ok(input) => input,
                Err(error) => return Box::pin(async move { Err(error) }),
            };
        },
    )
}

fn action_invocation(
    action: &ProgressiveAction,
    input: &TokenStream2,
    context: &TokenStream2,
) -> TokenStream2 {
    let ident = &action.method.sig.ident;
    let await_token = action.method.sig.asyncness.map(|_| quote! { .await });
    let args = method_call_args(&action.call, input, context);
    if action.call.has_receiver {
        quote! {
            {
                let server = self.clone();
                async move { server.#ident(#args)#await_token }
            }
        }
    } else {
        quote! {
            async move { Self::#ident(#args)#await_token }
        }
    }
}

fn method_call_args(
    call: &MethodCall,
    input: &TokenStream2,
    context: &TokenStream2,
) -> TokenStream2 {
    match (call.input_ty.is_some(), call.takes_context) {
        (true, true) => quote! { #input, #context },
        (true, false) => quote! { #input },
        (false, true) => quote! { #context },
        (false, false) => TokenStream2::new(),
    }
}

fn token_validator_closure(method: &ImplItemFn) -> TokenStream2 {
    let ident = &method.sig.ident;
    let await_token = method.sig.asyncness.map(|_| quote! { .await });
    if method.sig.receiver().is_some() {
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

fn helper_attr_name(attr: &Attribute) -> Option<String> {
    attr.path()
        .segments
        .last()
        .map(|segment| segment.ident.to_string())
        .filter(|name| matches!(name.as_str(), "action" | "token_validator"))
}

#[allow(clippy::too_many_lines)]
fn validate_progressive_parts(
    args: &ProgressiveArgs,
    actions: &[ProgressiveAction],
) -> syn::Result<()> {
    if args.tools.is_empty() {
        return Err(syn::Error::new(
            proc_macro2::Span::call_site(),
            "progressive servers must declare at least one `tool(...)`",
        ));
    }
    let mut tool_names = BTreeSet::new();
    for tool in &args.tools {
        let name = tool.name.value();
        if name.trim().is_empty() {
            return Err(syn::Error::new_spanned(
                &tool.name,
                "progressive tool names must not be empty",
            ));
        }
        if !tool_names.insert(name) {
            return Err(syn::Error::new_spanned(
                &tool.name,
                "duplicate progressive tool",
            ));
        }
        let mut namespaces = BTreeSet::new();
        for namespace in &tool.namespaces {
            let name = namespace.name.value();
            if namespace.explicit_name && !is_public_namespace_name(&name) {
                return Err(syn::Error::new_spanned(
                    &namespace.name,
                    "explicit progressive namespace names must use lowercase kebab-case (`[a-z0-9]+(?:-[a-z0-9]+)*`)",
                ));
            }
            if matches!(name.as_str(), "help" | "runbooks") {
                return Err(syn::Error::new_spanned(
                    &namespace.name,
                    "`help` and `runbooks` are reserved progressive namespaces",
                ));
            }
            if !namespaces.insert(name) {
                return Err(syn::Error::new_spanned(
                    &namespace.name,
                    "duplicate progressive namespace within tool",
                ));
            }
        }
    }
    let mut action_names = BTreeSet::new();
    for action in actions {
        let tool = args
            .tools
            .iter()
            .find(|tool| tool.name.value() == action.tool)
            .ok_or_else(|| {
                syn::Error::new_spanned(
                    &action.attr.tool,
                    format!(
                        "progressive action references undeclared tool `{}`",
                        action.tool
                    ),
                )
            })?;
        let name = action.name.value();
        if name.is_empty() {
            return Err(syn::Error::new_spanned(
                &action.name,
                "progressive action names must not be empty",
            ));
        }
        if matches!(name.as_str(), "help" | "runbooks")
            || name.starts_with("help.")
            || name.starts_with("runbooks.")
        {
            return Err(syn::Error::new_spanned(
                &action.name,
                "`help` and `runbooks` are reserved progressive action names",
            ));
        }
        if !action_names.insert((action.tool.clone(), name.clone())) {
            return Err(syn::Error::new_spanned(
                &action.name,
                "duplicate progressive action within tool",
            ));
        }
        if let Some((namespace, action_name)) = name.split_once('.') {
            if namespace.is_empty() || action_name.is_empty() || action_name.contains('.') {
                return Err(syn::Error::new_spanned(
                    &action.name,
                    "namespaced progressive actions must use `namespace.action`",
                ));
            }
            if !tool
                .namespaces
                .iter()
                .any(|declared| declared.name.value() == namespace)
            {
                return Err(syn::Error::new_spanned(
                    &action.name,
                    format!(
                        "progressive action `{name}` uses undeclared namespace `{namespace}` for tool `{}`",
                        action.tool
                    ),
                ));
            }
        }
        if action.attr.resource.is_some() && action.call.input_ty.is_some() {
            return Err(syn::Error::new_spanned(
                &action.name,
                "actions exposed as fixed resources cannot require input; use `resource_template = ...`",
            ));
        }
    }
    Ok(())
}

struct ProgressiveAction {
    method: ImplItemFn,
    attr: ActionAttr,
    docs: DocComment,
    tool: String,
    name: LitStr,
    call: MethodCall,
}

#[derive(Clone, Copy)]
struct ProgressiveParts<'a> {
    actions: &'a [ProgressiveAction],
}

impl ProgressiveAction {
    fn from_method(method: &ImplItemFn, attr: &Attribute) -> syn::Result<Self> {
        let attr = ActionAttr::from_attr(attr)?;
        let name = attr.name.clone();
        let docs = DocComment::from_attrs(&method.attrs).ok_or_else(|| {
            syn::Error::new_spanned(
                method,
                format!(
                    "progressive action `{}` must have a doc comment",
                    name.value()
                ),
            )
        })?;
        Ok(Self {
            call: MethodCall::from_method(method)?,
            method: method.clone(),
            tool: attr.tool.value(),
            attr,
            docs,
            name,
        })
    }

    const fn exposes_resource(&self) -> bool {
        self.attr.resource.is_some() || self.attr.resource_template.is_some()
    }
}

struct MethodCall {
    has_receiver: bool,
    input_ty: Option<Type>,
    takes_context: bool,
}

impl MethodCall {
    fn from_method(method: &ImplItemFn) -> syn::Result<Self> {
        let has_receiver = method.sig.receiver().is_some();
        let args = method
            .sig
            .inputs
            .iter()
            .filter_map(|arg| match arg {
                FnArg::Receiver(_) => None,
                FnArg::Typed(arg) => Some(arg),
            })
            .collect::<Vec<_>>();
        if args.len() > 2 {
            return Err(syn::Error::new_spanned(
                method,
                "progressive methods accept at most one input argument and one ServerContext argument",
            ));
        }
        let mut input_ty = None;
        let mut takes_context = false;
        for arg in args {
            if type_is_server_context(&arg.ty) {
                if takes_context {
                    return Err(syn::Error::new_spanned(
                        arg,
                        "duplicate ServerContext argument",
                    ));
                }
                takes_context = true;
            } else {
                if input_ty.is_some() {
                    return Err(syn::Error::new_spanned(
                        arg,
                        "duplicate action input argument",
                    ));
                }
                input_ty = Some((*arg.ty).clone());
            }
        }
        Ok(Self {
            has_receiver,
            input_ty,
            takes_context,
        })
    }
}

fn type_is_server_context(ty: &Type) -> bool {
    let Type::Path(path) = ty else {
        return false;
    };
    path.path
        .segments
        .last()
        .is_some_and(|segment| segment.ident == "ServerContext")
}

struct ActionAttr {
    tool: LitStr,
    name: LitStr,
    resource: Option<LitStr>,
    resource_template: Option<LitStr>,
}

impl ActionAttr {
    fn from_attr(attr: &Attribute) -> syn::Result<Self> {
        let mut name = None;
        let mut tool = None;
        let mut resource = None;
        let mut resource_template = None;
        attr.parse_nested_meta(|meta| {
            if meta.path.is_ident("tool") {
                tool = Some(meta.value()?.parse()?);
            } else if meta.path.is_ident("name") {
                name = Some(meta.value()?.parse()?);
            } else if meta.path.is_ident("resource") {
                resource = Some(meta.value()?.parse()?);
            } else if meta.path.is_ident("resource_template") {
                resource_template = Some(meta.value()?.parse()?);
            } else {
                return Err(meta.error("unsupported progressive action argument"));
            }
            Ok(())
        })?;
        let name = name.ok_or_else(|| syn::Error::new_spanned(attr, "missing `name = ...`"))?;
        let tool = tool.ok_or_else(|| syn::Error::new_spanned(attr, "missing `tool = ...`"))?;
        Ok(Self {
            tool,
            name,
            resource,
            resource_template,
        })
    }
}

struct DocComment {
    description: String,
}

impl DocComment {
    fn from_attrs(attrs: &[Attribute]) -> Option<Self> {
        let lines = attrs
            .iter()
            .filter_map(|attr| match &attr.meta {
                Meta::NameValue(name_value) if name_value.path.is_ident("doc") => {
                    let Expr::Lit(expr_lit) = &name_value.value else {
                        return None;
                    };
                    let Lit::Str(line) = &expr_lit.lit else {
                        return None;
                    };
                    Some(line.value().trim().to_owned())
                }
                _ => None,
            })
            .collect::<Vec<_>>();
        let description = lines.iter().find(|line| !line.is_empty())?.clone();
        Some(Self { description })
    }
}

#[allow(clippy::redundant_pub_crate)]
pub(crate) struct ProgressiveArgs {
    name: Expr,
    version: Expr,
    description: Option<Expr>,
    skills: Option<Expr>,
    tools: Vec<ProgressiveToolArg>,
    auth: AuthArgs,
}

impl syn::parse::Parse for ProgressiveArgs {
    fn parse(input: syn::parse::ParseStream<'_>) -> syn::Result<Self> {
        let metas = Punctuated::<Meta, Token![,]>::parse_terminated(input)?;
        let mut args = Self {
            name: syn::parse_quote!("mcp-progressive-server"),
            version: syn::parse_quote!("0"),
            description: None,
            skills: None,
            tools: Vec::new(),
            auth: AuthArgs::default(),
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
                Meta::NameValue(value) if path_is(&value.path, "skills") => {
                    args.skills = Some(value.value);
                }
                Meta::List(list) if path_is(&list.path, "tool") => {
                    args.tools
                        .push(ProgressiveToolArg::from_tokens(list.tokens)?);
                }
                Meta::List(list) if path_is(&list.path, "auth") => {
                    args.auth = AuthArgs::from_tokens(list.tokens)?;
                }
                other => {
                    return Err(syn::Error::new_spanned(
                        other,
                        "unsupported progressive_server argument",
                    ));
                }
            }
        }
        Ok(args)
    }
}

struct ProgressiveToolArg {
    name: LitStr,
    description: LitStr,
    annotations: Option<Expr>,
    namespaces: Vec<ProgressiveNamespaceArg>,
}

impl ProgressiveToolArg {
    fn from_tokens(tokens: TokenStream2) -> syn::Result<Self> {
        let parser = Punctuated::<Meta, Token![,]>::parse_terminated;
        let metas = parser.parse2(tokens)?;
        let mut name = None;
        let mut description = None;
        let mut annotations = None;
        let mut namespaces = Vec::new();
        for meta in metas {
            match meta {
                Meta::NameValue(value) if path_is(&value.path, "name") => {
                    name = Some(lit_str(value.value, "tool name")?);
                }
                Meta::NameValue(value) if path_is(&value.path, "description") => {
                    description = Some(lit_str(value.value, "tool description")?);
                }
                Meta::NameValue(value) if path_is(&value.path, "annotations") => {
                    if annotations.is_some() {
                        return Err(syn::Error::new_spanned(
                            value,
                            "duplicate progressive tool `annotations`",
                        ));
                    }
                    annotations = Some(value.value);
                }
                Meta::List(list) if path_is(&list.path, "namespace") => {
                    namespaces.push(ProgressiveNamespaceArg::from_tokens(list.tokens)?);
                }
                other => {
                    return Err(syn::Error::new_spanned(
                        other,
                        "unsupported progressive tool argument",
                    ));
                }
            }
        }
        Ok(Self {
            name: name.ok_or_else(|| {
                syn::Error::new(
                    proc_macro2::Span::call_site(),
                    "progressive tool requires `name = ...`",
                )
            })?,
            description: description.ok_or_else(|| {
                syn::Error::new(
                    proc_macro2::Span::call_site(),
                    "progressive tool requires `description = ...`",
                )
            })?,
            annotations,
            namespaces,
        })
    }
}

struct ProgressiveNamespaceArg {
    name: LitStr,
    explicit_name: bool,
    description: Expr,
}

fn lit_str(expr: Expr, field: &str) -> syn::Result<LitStr> {
    let Expr::Lit(expr_lit) = expr else {
        return Err(syn::Error::new_spanned(
            expr,
            format!("{field} must be a string literal"),
        ));
    };
    let Lit::Str(value) = expr_lit.lit else {
        return Err(syn::Error::new_spanned(
            expr_lit,
            format!("{field} must be a string literal"),
        ));
    };
    Ok(value)
}

impl ProgressiveNamespaceArg {
    fn from_tokens(tokens: TokenStream2) -> syn::Result<Self> {
        let parser = |input: syn::parse::ParseStream<'_>| {
            let first = input.parse::<syn::Ident>()?;
            let (name, explicit_name) = if first == "name" && input.peek(Token![=]) {
                input.parse::<Token![=]>()?;
                let name = lit_str(input.parse::<Expr>()?, "namespace name")?;
                (name, true)
            } else {
                (LitStr::new(&first.to_string(), first.span()), false)
            };
            input.parse::<Token![,]>()?;
            let description_ident = input.parse::<syn::Ident>()?;
            if description_ident != "description" {
                return Err(syn::Error::new_spanned(
                    description_ident,
                    "expected `description = ...`",
                ));
            }
            input.parse::<Token![=]>()?;
            let description = input.parse::<Expr>()?;
            Ok(Self {
                name,
                explicit_name,
                description,
            })
        };
        parser.parse2(tokens)
    }
}

fn is_public_namespace_name(name: &str) -> bool {
    !name.is_empty()
        && name.split('-').all(|segment| {
            !segment.is_empty()
                && segment
                    .bytes()
                    .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit())
        })
}

fn path_is(path: &Path, ident: &str) -> bool {
    path.is_ident(ident)
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rejects_missing_duplicate_and_empty_tools() {
        assert_error_contains("", "progressive servers must declare at least one");
        assert_error_contains(
            r#"tool(name = "same", description = "One"), tool(name = "same", description = "Two")"#,
            "duplicate progressive tool",
        );
        assert_error_contains(
            r#"tool(name = "", description = "Empty")"#,
            "tool names must not be empty",
        );
        let error = syn::parse_str::<ProgressiveArgs>(r#"tool(name = "missing")"#)
            .err()
            .expect("missing description must fail");
        assert!(error.to_string().contains("requires `description"));
    }

    #[test]
    fn accepts_annotation_expressions_and_rejects_duplicates() {
        let args = syn::parse_str::<ProgressiveArgs>(
            r#"tool(
                name = "exec",
                description = "Exec",
                annotations = annotations_for("exec")
            )"#,
        )
        .expect("annotation expression");
        let input = syn::parse_str::<ItemImpl>("impl Server {}").expect("impl");
        let expanded = expand_progressive_server(&args, input)
            .expect("expansion")
            .to_string();
        assert!(expanded.contains("annotations_for (\"exec\")"));

        let error = syn::parse_str::<ProgressiveArgs>(
            r#"tool(
                name = "exec",
                description = "Exec",
                annotations = first(),
                annotations = second()
            )"#,
        )
        .err()
        .expect("duplicate annotations must fail");
        assert!(
            error
                .to_string()
                .contains("duplicate progressive tool `annotations`")
        );
    }

    #[test]
    fn rejects_undeclared_tool_references() {
        assert_impl_error_contains(
            one_tool_args(),
            r#"
                impl Server {
                    /// Action.
                    #[action(tool = "other", name = "inspect")]
                    fn inspect() {}
                }
            "#,
            "action references undeclared tool `other`",
        );
    }

    #[test]
    fn rejects_undeclared_and_reserved_namespaces() {
        assert_impl_error_contains(
            one_tool_args(),
            r#"
                impl Server {
                    /// Action.
                    #[action(tool = "exec", name = "missing.inspect")]
                    fn inspect() {}
                }
            "#,
            "uses undeclared namespace `missing`",
        );
        assert_error_contains(
            r#"tool(name = "exec", description = "Exec", namespace(runbooks, description = "Reserved"))"#,
            "reserved progressive namespaces",
        );
        assert_error_contains(
            r#"tool(name = "exec", description = "Exec", namespace(name = "help", description = "Reserved"))"#,
            "reserved progressive namespaces",
        );
        assert_impl_error_contains(
            one_tool_args(),
            r#"
                impl Server {
                    /// Action.
                    #[action(tool = "exec", name = "help.inspect")]
                    fn inspect() {}
                }
            "#,
            "reserved progressive action names",
        );
    }

    #[test]
    fn accepts_explicit_kebab_case_namespaces_and_preserves_identifier_namespaces() {
        let args = syn::parse_str::<ProgressiveArgs>(
            r#"tool(
                name = "exec",
                description = "Exec",
                namespace(releases, description = "Releases"),
                namespace(name = "alert-rule", description = "Alert rules"),
                namespace(name = "alert-instance", description = "Alert instances")
            )"#,
        )
        .expect("namespace declarations");
        let input = syn::parse_str::<ItemImpl>(
            r#"
                impl Server {
                    /// List alert rules.
                    #[action(tool = "exec", name = "alert-rule.list")]
                    fn list_alert_rules() {}
                }
            "#,
        )
        .expect("impl");

        expand_progressive_server(&args, input).expect("kebab-case namespace must expand");
    }

    #[test]
    fn rejects_invalid_explicit_namespace_names() {
        for name in [
            "",
            "alert.rule",
            "alert rule",
            "Alert-rule",
            "alert_rule",
            "-alert-rule",
            "alert-rule-",
            "alert--rule",
        ] {
            assert_error_contains(
                &format!(
                    r#"tool(name = "exec", description = "Exec", namespace(name = "{name}", description = "Invalid"))"#
                ),
                "must use lowercase kebab-case",
            );
        }
    }

    #[test]
    fn explicit_and_identifier_namespace_names_share_duplicate_checks() {
        assert_error_contains(
            r#"tool(
                name = "exec",
                description = "Exec",
                namespace(releases, description = "Identifier"),
                namespace(name = "releases", description = "Explicit")
            )"#,
            "duplicate progressive namespace within tool",
        );
    }

    #[test]
    fn explicit_namespaces_require_exact_roots_and_one_action_separator() {
        let args = r#"tool(
            name = "exec",
            description = "Exec",
            namespace(name = "alert-rule", description = "Alert rules")
        )"#;
        assert_impl_error_contains(
            args,
            r#"
                impl Server {
                    /// List alerts.
                    #[action(tool = "exec", name = "alert.list")]
                    fn list() {}
                }
            "#,
            "uses undeclared namespace `alert`",
        );
        assert_impl_error_contains(
            args,
            r#"
                impl Server {
                    /// List alerts.
                    #[action(tool = "exec", name = "alert-rule.list.all")]
                    fn list() {}
                }
            "#,
            "must use `namespace.action`",
        );
    }

    #[test]
    fn action_names_are_unique_per_tool_only() {
        let args = syn::parse_str::<ProgressiveArgs>(
            r#"tool(name = "one", description = "One"), tool(name = "two", description = "Two")"#,
        )
        .expect("args");
        let input = syn::parse_str::<ItemImpl>(
            r#"
                impl Server {
                    /// First.
                    #[action(tool = "one", name = "inspect")]
                    fn first() {}
                    /// Second.
                    #[action(tool = "two", name = "inspect")]
                    fn second() {}
                }
            "#,
        )
        .expect("impl");
        expand_progressive_server(&args, input).expect("same action name across tools is valid");

        assert_impl_error_contains(
            one_tool_args(),
            r#"
                impl Server {
                    /// First.
                    #[action(tool = "exec", name = "inspect")]
                    fn first() {}
                    /// Second.
                    #[action(tool = "exec", name = "inspect")]
                    fn second() {}
                }
            "#,
            "duplicate progressive action within tool",
        );
    }

    #[test]
    fn rejects_removed_runbook_options_and_registers_catalog() {
        assert!(syn::parse_str::<ProgressiveArgs>(r#"tool(name = "exec", description = "Exec", runbook_uri_template = "x://{runbook}")"#).is_err());
        let args = syn::parse_str::<ProgressiveArgs>(
            r#"skills = self.catalog, tool(name = "exec", description = "Exec")"#,
        )
        .unwrap();
        let expanded = expand_progressive_server(&args, syn::parse_str("impl Server {}").unwrap())
            .unwrap()
            .to_string();
        assert!(expanded.contains("skill_catalog"));
        assert!(!expanded.contains("root_help"));
    }

    fn one_tool_args() -> &'static str {
        r#"tool(name = "exec", description = "Exec")"#
    }

    fn assert_error_contains(args: &str, expected: &str) {
        assert_impl_error_contains(args, "impl Server {}", expected);
    }

    fn assert_impl_error_contains(args: &str, input: &str, expected: &str) {
        let args = syn::parse_str::<ProgressiveArgs>(args).expect("valid test args");
        let input = syn::parse_str::<ItemImpl>(input).expect("valid test impl");
        let error = expand_progressive_server(&args, input).expect_err("expansion must fail");
        assert!(
            error.to_string().contains(expected),
            "expected `{expected}` in `{error}`"
        );
    }
}
