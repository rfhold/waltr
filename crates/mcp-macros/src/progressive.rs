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
    let mut workflows = Vec::new();
    let mut token_validator = None;

    for item in &mut input.items {
        let ImplItem::Fn(method) = item else {
            continue;
        };

        let mut retained_attrs = Vec::new();
        for attr in &method.attrs {
            match helper_attr_name(attr).as_deref() {
                Some("action") => actions.push(ProgressiveAction::from_method(method, attr)?),
                Some("workflow") => workflows.push(ProgressiveWorkflow::from_method(method, attr)?),
                Some("token_validator") => token_validator = Some(method.clone()),
                _ => retained_attrs.push(attr.clone()),
            }
        }
        method.attrs = retained_attrs;
    }

    validate_progressive_parts(args, &actions, &workflows)?;

    let handler_impl = handler_impl(
        args,
        &self_ty,
        &impl_generics,
        where_clause,
        ProgressiveParts {
            actions: &actions,
            workflows: &workflows,
        },
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
    let ProgressiveParts { actions, workflows } = parts;
    let server_info = server_info_impl(args);
    let capabilities = capabilities_impl(actions, workflows);
    let instructions = instructions_impl(args);
    let list_tools = list_tools_impl(actions, workflows);
    let call_tool = call_tool_impl(args, actions, workflows);
    let list_prompts = list_prompts_impl(workflows);
    let get_prompt = get_prompt_impl(workflows);
    let list_resources = list_resources_impl(actions);
    let list_resource_templates = list_resource_templates_impl(args, actions, workflows);
    let read_resource = read_resource_impl(args, actions, workflows);

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

fn capabilities_impl(
    actions: &[ProgressiveAction],
    workflows: &[ProgressiveWorkflow],
) -> TokenStream2 {
    let prompts =
        (!workflows.is_empty()).then(|| quote! { let capabilities = capabilities.prompts(); });
    let resources = (actions.iter().any(ProgressiveAction::exposes_resource)
        || !workflows.is_empty())
    .then(|| quote! { let capabilities = capabilities.resources(); });
    quote! {
        fn capabilities(&self) -> ::mcp::server::ServerCapabilities {
            let capabilities = ::mcp::server::ServerCapabilities::new().tools();
            #prompts
            #resources
            capabilities
        }
    }
}

fn instructions_impl(args: &ProgressiveArgs) -> TokenStream2 {
    let description = optional_string_expr(args.description.as_ref());
    quote! {
        fn instructions(&self) -> Option<String> {
            let description = #description;
            Some(::mcp::progressive::instructions(description.as_deref()))
        }
    }
}

fn list_tools_impl(
    actions: &[ProgressiveAction],
    workflows: &[ProgressiveWorkflow],
) -> TokenStream2 {
    let input_schema = exec_input_schema_expr(actions, workflows);
    quote! {
        fn list_tools(
            &self,
            _cursor: Option<String>,
            _context: ::mcp::server::ServerContext,
        ) -> ::mcp::server::BoxFuture<::mcp::server::ServerResult<::mcp::McpToolList>> {
            Box::pin(async {
                Ok(::mcp::McpToolList {
                    tools: vec![::mcp::progressive::exec_tool_definition(#input_schema)],
                    next_cursor: None,
                })
            })
        }
    }
}

fn call_tool_impl(
    args: &ProgressiveArgs,
    actions: &[ProgressiveAction],
    workflows: &[ProgressiveWorkflow],
) -> TokenStream2 {
    let root_help = root_help_expr(args, actions, workflows);
    let workflows_help = workflows_help_expr(args, workflows);
    let namespace_branches = args.namespaces.iter().map(|namespace| {
        let help_action = format!("help.{}", namespace.name);
        let help = namespace_help_expr(namespace, actions);
        quote! {
            #help_action => {
                return Box::pin(async move {
                    ::mcp::progressive::tool_result(#help, request.filter.as_deref())
                });
            }
        }
    });
    let action_branches = actions.iter().map(|action| {
        let name = action.name.value();
        let call = action_tool_call(action);
        quote! {
            #name => {
                return #call;
            }
        }
    });
    let workflow_load = workflow_load_call(workflows);

    quote! {
        fn call_tool(
            &self,
            call: ::mcp::McpToolCall,
            context: ::mcp::server::ServerContext,
        ) -> ::mcp::server::BoxFuture<::mcp::server::ServerResult<::mcp::McpToolResult>> {
            if call.name != ::mcp::progressive::EXEC_TOOL_NAME {
                return Box::pin(async move {
                    Err(::mcp::server::ServerError::invalid_params(format!(
                        "unknown tool `{}`; progressive servers only expose `exec`",
                        call.name
                    )))
                });
            }
            let request = match ::mcp::progressive::parse_exec_arguments(call.arguments) {
                Ok(request) => request,
                Err(error) => return Box::pin(async move { Err(error) }),
            };
            match request.action.as_str() {
                "help" => {
                    return Box::pin(async move {
                        ::mcp::progressive::tool_result(#root_help, request.filter.as_deref())
                    });
                }
                "help.workflows" => {
                    return Box::pin(async move {
                        ::mcp::progressive::tool_result(#workflows_help, request.filter.as_deref())
                    });
                }
                "workflows.load" => {
                    return #workflow_load;
                }
                #(#namespace_branches)*
                #(#action_branches)*
                _ => {}
            }
            Box::pin(async move { Err(::mcp::progressive::unknown_action(&request.action)) })
        }
    }
}

fn list_prompts_impl(workflows: &[ProgressiveWorkflow]) -> TokenStream2 {
    if workflows.is_empty() {
        return TokenStream2::new();
    }
    let definitions = workflows.iter().map(|workflow| {
        let name = &workflow.name;
        let description = &workflow.docs.description;
        quote! { ::mcp::progressive::workflow_prompt_definition(#name, #description) }
    });
    quote! {
        fn list_prompts(
            &self,
            _cursor: Option<String>,
            _context: ::mcp::server::ServerContext,
        ) -> ::mcp::server::BoxFuture<::mcp::server::ServerResult<::mcp::McpPromptList>> {
            Box::pin(async {
                Ok(::mcp::McpPromptList {
                    prompts: vec![#(#definitions),*],
                    next_cursor: None,
                })
            })
        }
    }
}

fn get_prompt_impl(workflows: &[ProgressiveWorkflow]) -> TokenStream2 {
    if workflows.is_empty() {
        return TokenStream2::new();
    }
    let branches = workflows.iter().map(|workflow| {
        let name = &workflow.name;
        let call = workflow_prompt_call(workflow);
        quote! {
            if name == #name {
                return #call;
            }
        }
    });
    quote! {
        fn get_prompt(
            &self,
            name: String,
            _arguments: Option<::mcp::server::JsonValue>,
            context: ::mcp::server::ServerContext,
        ) -> ::mcp::server::BoxFuture<::mcp::server::ServerResult<::mcp::McpPromptResult>> {
            #(#branches)*
            Box::pin(async move {
                Err(::mcp::server::ServerError::invalid_params(format!(
                    "unknown workflow prompt `{name}`; call `help.workflows`"
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

fn list_resource_templates_impl(
    args: &ProgressiveArgs,
    actions: &[ProgressiveAction],
    workflows: &[ProgressiveWorkflow],
) -> TokenStream2 {
    let action_templates = actions.iter().filter_map(|action| {
        let uri_template = action.attr.resource_template.as_ref()?;
        let name = &action.name;
        let description = &action.docs.description;
        Some(quote! { ::mcp::progressive::action_resource_template_definition(#uri_template, #name, #description) })
    });
    let workflow_template = (!workflows.is_empty()).then(|| {
        let uri_template = &args.workflow_uri_template;
        quote! { templates.push(::mcp::progressive::workflow_resource_template_definition(#uri_template)); }
    });
    quote! {
        fn list_resource_templates(
            &self,
            _cursor: Option<String>,
            _context: ::mcp::server::ServerContext,
        ) -> ::mcp::server::BoxFuture<::mcp::server::ServerResult<::mcp::McpResourceTemplateList>> {
            Box::pin(async {
                let mut templates = vec![#(#action_templates),*];
                #workflow_template
                Ok(::mcp::McpResourceTemplateList {
                    resource_templates: templates,
                    next_cursor: None,
                })
            })
        }
    }
}

fn read_resource_impl(
    args: &ProgressiveArgs,
    actions: &[ProgressiveAction],
    workflows: &[ProgressiveWorkflow],
) -> TokenStream2 {
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
    let workflow_template = &args.workflow_uri_template;
    let workflow_branch =
        (!workflows.is_empty()).then(|| workflow_resource_call(workflows, workflow_template));
    quote! {
        fn read_resource(
            &self,
            uri: String,
            context: ::mcp::server::ServerContext,
        ) -> ::mcp::server::BoxFuture<::mcp::server::ServerResult<::mcp::McpResourceResult>> {
            #(#resource_branches)*
            #(#template_branches)*
            #workflow_branch
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

fn exec_input_schema_expr(
    actions: &[ProgressiveAction],
    workflows: &[ProgressiveWorkflow],
) -> TokenStream2 {
    let action_names = actions.iter().map(|action| &action.name);
    let workflow_names = workflows.iter().map(|workflow| &workflow.name);
    let inputs = actions.iter().map(|action| {
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
        ::mcp::progressive::exec_input_schema(
            &[#(#action_names),*],
            vec![#(#inputs),*],
            &[#(#workflow_names),*],
        )
    }
}

fn root_help_expr(
    args: &ProgressiveArgs,
    actions: &[ProgressiveAction],
    workflows: &[ProgressiveWorkflow],
) -> TokenStream2 {
    let description = optional_string_expr(args.description.as_ref());
    let namespaces = namespaces_expr(args);
    let workflow_help = workflows_help_vec_expr(workflows);
    let _ = actions;
    quote! {
        {
            let description = #description;
            ::mcp::progressive::root_help(description.as_deref(), &#namespaces, &#workflow_help)
        }
    }
}

fn namespace_help_expr(
    namespace: &ProgressiveNamespaceArg,
    actions: &[ProgressiveAction],
) -> TokenStream2 {
    let namespace_value = namespace_expr(namespace);
    let action_values = actions
        .iter()
        .filter(|action| namespace.name == action.namespace)
        .map(action_help_expr);
    quote! {
        {
            let namespace = #namespace_value;
            let actions = vec![#(#action_values),*];
            ::mcp::progressive::namespace_help(&namespace, &actions)
        }
    }
}

fn workflows_help_expr(args: &ProgressiveArgs, workflows: &[ProgressiveWorkflow]) -> TokenStream2 {
    let workflow_uri_template = &args.workflow_uri_template;
    let workflow_help = workflows_help_vec_expr(workflows);
    quote! {
        {
            let workflows = #workflow_help;
            ::mcp::progressive::workflows_help(#workflow_uri_template, &workflows)
        }
    }
}

fn namespaces_expr(args: &ProgressiveArgs) -> TokenStream2 {
    let namespaces = args.namespaces.iter().map(namespace_expr);
    quote! { vec![#(#namespaces),*] }
}

fn namespace_expr(namespace: &ProgressiveNamespaceArg) -> TokenStream2 {
    let name = namespace.name.to_string();
    let description = &namespace.description;
    quote! {
        ::mcp::progressive::ProgressiveNamespace {
            name: #name,
            description: ::std::string::String::from(#description),
        }
    }
}

fn action_help_expr(action: &ProgressiveAction) -> TokenStream2 {
    let name = &action.name;
    let namespace = &action.namespace;
    let description = &action.docs.description;
    let guidance = &action.docs.guidance;
    let resource = optional_litstr_expr(action.attr.resource.as_ref());
    let resource_template = optional_litstr_expr(action.attr.resource_template.as_ref());
    let input_schema = action.call.input_ty.as_ref().map_or_else(
        || quote! { None },
        |input_ty| quote! { Some(::mcp::progressive::json_schema_for::<#input_ty>()) },
    );
    quote! {
        ::mcp::progressive::ProgressiveActionHelp {
            name: #name,
            namespace: #namespace,
            description: ::std::string::String::from(#description),
            guidance: ::std::string::String::from(#guidance),
            input_schema: #input_schema,
            resource: #resource,
            resource_template: #resource_template,
        }
    }
}

fn workflows_help_vec_expr(workflows: &[ProgressiveWorkflow]) -> TokenStream2 {
    let workflows = workflows.iter().map(|workflow| {
        let name = &workflow.name;
        let description = &workflow.docs.description;
        let guidance = &workflow.docs.guidance;
        quote! {
            ::mcp::progressive::ProgressiveWorkflowHelp {
                name: #name,
                description: ::std::string::String::from(#description),
                guidance: ::std::string::String::from(#guidance),
            }
        }
    });
    quote! { vec![#(#workflows),*] }
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
                ::mcp::progressive::tool_result(output, request.filter.as_deref())
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
                Ok(::mcp::progressive::json_resource_result(#uri, output))
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
    let namespace = &action.namespace;
    let invocation = action_invocation(action, &quote! { input }, &quote! { context });
    let input_prepare = action.call.input_ty.as_ref().map_or_else(
        || quote! { let input = (); },
        |input_ty| quote! {
            let Some(bindings) = ::mcp::progressive::template_bindings(#uri_template, &uri) else {
                return Box::pin(async move { Err(::mcp::server::ServerError::resource_not_found(uri)) });
            };
            let input = match ::mcp::progressive::deserialize_action_input::<#input_ty>(
                #action_name,
                #namespace,
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
                Ok(::mcp::progressive::json_resource_result(#uri, output))
            }) as ::mcp::server::BoxFuture<::mcp::server::ServerResult<::mcp::McpResourceResult>>
        }
    }
}

fn workflow_load_call(workflows: &[ProgressiveWorkflow]) -> TokenStream2 {
    let branches = workflows.iter().map(|workflow| {
        let name = &workflow.name;
        let call = workflow_string_future(workflow, &quote! { context });
        quote! {
            #name => {
                let future = #call;
                return Box::pin(async move {
                    let markdown = future.await?;
                    ::mcp::progressive::workflow_tool_result(#name, markdown, request.filter.as_deref())
                }) as ::mcp::server::BoxFuture<::mcp::server::ServerResult<::mcp::McpToolResult>>;
            }
        }
    });
    quote! {
        {
            let workflow = match ::mcp::progressive::workflow_load_input(request.input) {
                Ok(workflow) => workflow,
                Err(error) => return Box::pin(async move { Err(error) }),
            };
            match workflow.as_str() {
                #(#branches)*
                _ => {}
            }
            Box::pin(async move { Err(::mcp::progressive::unknown_workflow(&workflow)) })
        }
    }
}

fn workflow_prompt_call(workflow: &ProgressiveWorkflow) -> TokenStream2 {
    let call = workflow_string_future(workflow, &quote! { context });
    quote! {
        {
            let future = #call;
            Box::pin(async move {
                let markdown = future.await?;
                Ok(::mcp::progressive::workflow_prompt_result(markdown))
            }) as ::mcp::server::BoxFuture<::mcp::server::ServerResult<::mcp::McpPromptResult>>
        }
    }
}

fn workflow_resource_call(
    workflows: &[ProgressiveWorkflow],
    workflow_uri_template: &LitStr,
) -> TokenStream2 {
    let branches = workflows.iter().map(|workflow| {
        let name = &workflow.name;
        let call = workflow_string_future(workflow, &quote! { context });
        quote! {
            #name => {
                let future = #call;
                return Box::pin(async move {
                    let markdown = future.await?;
                    Ok(::mcp::progressive::workflow_resource_result(uri, markdown))
                }) as ::mcp::server::BoxFuture<::mcp::server::ServerResult<::mcp::McpResourceResult>>;
            }
        }
    });
    quote! {
        if let Some(workflow) = ::mcp::progressive::workflow_from_uri(#workflow_uri_template, &uri) {
            match workflow.as_str() {
                #(#branches)*
                _ => {}
            }
            return Box::pin(async move { Err(::mcp::progressive::unknown_workflow(&workflow)) });
        }
    }
}

fn action_input_prepare(action: &ProgressiveAction, input: &TokenStream2) -> TokenStream2 {
    let action_name = action.name.value();
    let namespace = &action.namespace;
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
                #namespace,
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

fn workflow_string_future(workflow: &ProgressiveWorkflow, context: &TokenStream2) -> TokenStream2 {
    let ident = &workflow.method.sig.ident;
    let await_token = workflow.method.sig.asyncness.map(|_| quote! { .await });
    let args = method_call_args(&workflow.call, &quote! { () }, context);
    if workflow.call.has_receiver {
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

fn optional_string_expr(expr: Option<&Expr>) -> TokenStream2 {
    expr.map_or_else(
        || quote! { None::<String> },
        |expr| quote! { Some(::std::string::String::from(#expr)) },
    )
}

fn optional_litstr_expr(value: Option<&LitStr>) -> TokenStream2 {
    value.map_or_else(|| quote! { None }, |value| quote! { Some(#value) })
}

fn helper_attr_name(attr: &Attribute) -> Option<String> {
    attr.path()
        .segments
        .last()
        .map(|segment| segment.ident.to_string())
        .filter(|name| matches!(name.as_str(), "action" | "workflow" | "token_validator"))
}

fn validate_progressive_parts(
    args: &ProgressiveArgs,
    actions: &[ProgressiveAction],
    workflows: &[ProgressiveWorkflow],
) -> syn::Result<()> {
    let namespaces = args
        .namespaces
        .iter()
        .map(|namespace| namespace.name.to_string())
        .collect::<BTreeSet<_>>();
    let mut action_names = BTreeSet::new();
    for action in actions {
        let name = action.name.value();
        if !action_names.insert(name.clone()) {
            return Err(syn::Error::new_spanned(
                &action.name,
                "duplicate progressive action",
            ));
        }
        let Some((namespace, _)) = name.split_once('.') else {
            return Err(syn::Error::new_spanned(
                &action.name,
                "progressive action names must be dot-notated, such as `namespace.action`",
            ));
        };
        if matches!(namespace, "help" | "workflows") {
            return Err(syn::Error::new_spanned(
                &action.name,
                "`help` and `workflows` are reserved progressive namespaces",
            ));
        }
        if !namespaces.contains(namespace) {
            return Err(syn::Error::new_spanned(
                &action.name,
                format!("progressive action `{name}` uses undeclared namespace `{namespace}`"),
            ));
        }
        if action.attr.resource.is_some() && action.call.input_ty.is_some() {
            return Err(syn::Error::new_spanned(
                &action.name,
                "actions exposed as fixed resources cannot require input; use `resource_template = ...`",
            ));
        }
    }
    let mut workflow_names = BTreeSet::new();
    for workflow in workflows {
        if !workflow_names.insert(workflow.name.value()) {
            return Err(syn::Error::new_spanned(
                &workflow.name,
                "duplicate progressive workflow",
            ));
        }
    }
    Ok(())
}

struct ProgressiveAction {
    method: ImplItemFn,
    attr: ActionAttr,
    docs: DocComment,
    namespace: String,
    name: LitStr,
    call: MethodCall,
}

#[derive(Clone, Copy)]
struct ProgressiveParts<'a> {
    actions: &'a [ProgressiveAction],
    workflows: &'a [ProgressiveWorkflow],
}

impl ProgressiveAction {
    fn from_method(method: &ImplItemFn, attr: &Attribute) -> syn::Result<Self> {
        let attr = ActionAttr::from_attr(attr)?;
        let name = attr.name.clone();
        let namespace = name
            .value()
            .split_once('.')
            .map(|(namespace, _)| namespace.to_owned())
            .unwrap_or_default();
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
            call: MethodCall::from_method(method, true)?,
            method: method.clone(),
            attr,
            docs,
            namespace,
            name,
        })
    }

    const fn exposes_resource(&self) -> bool {
        self.attr.resource.is_some() || self.attr.resource_template.is_some()
    }
}

struct ProgressiveWorkflow {
    method: ImplItemFn,
    name: LitStr,
    docs: DocComment,
    call: MethodCall,
}

impl ProgressiveWorkflow {
    fn from_method(method: &ImplItemFn, attr: &Attribute) -> syn::Result<Self> {
        let attr = WorkflowAttr::from_attr(attr)?;
        let docs = DocComment::from_attrs(&method.attrs).ok_or_else(|| {
            syn::Error::new_spanned(
                method,
                format!(
                    "progressive workflow `{}` must have a doc comment",
                    attr.name.value()
                ),
            )
        })?;
        Ok(Self {
            call: MethodCall::from_method(method, false)?,
            method: method.clone(),
            name: attr.name,
            docs,
        })
    }
}

struct MethodCall {
    has_receiver: bool,
    input_ty: Option<Type>,
    takes_context: bool,
}

impl MethodCall {
    fn from_method(method: &ImplItemFn, allow_input: bool) -> syn::Result<Self> {
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
            } else if allow_input {
                if input_ty.is_some() {
                    return Err(syn::Error::new_spanned(
                        arg,
                        "duplicate action input argument",
                    ));
                }
                input_ty = Some((*arg.ty).clone());
            } else {
                return Err(syn::Error::new_spanned(
                    arg,
                    "workflow methods do not accept action input; use only optional ServerContext",
                ));
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
    name: LitStr,
    resource: Option<LitStr>,
    resource_template: Option<LitStr>,
}

impl ActionAttr {
    fn from_attr(attr: &Attribute) -> syn::Result<Self> {
        let mut name = None;
        let mut resource = None;
        let mut resource_template = None;
        attr.parse_nested_meta(|meta| {
            if meta.path.is_ident("name") {
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
        Ok(Self {
            name,
            resource,
            resource_template,
        })
    }
}

struct WorkflowAttr {
    name: LitStr,
}

impl WorkflowAttr {
    fn from_attr(attr: &Attribute) -> syn::Result<Self> {
        let mut name = None;
        attr.parse_nested_meta(|meta| {
            if meta.path.is_ident("name") {
                name = Some(meta.value()?.parse()?);
                Ok(())
            } else {
                Err(meta.error("unsupported progressive workflow argument"))
            }
        })?;
        Ok(Self {
            name: name.ok_or_else(|| syn::Error::new_spanned(attr, "missing `name = ...`"))?,
        })
    }
}

struct DocComment {
    description: String,
    guidance: String,
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
        let mut seen_description = false;
        let guidance = lines
            .into_iter()
            .filter_map(|line| {
                if !seen_description && line == description {
                    seen_description = true;
                    return None;
                }
                seen_description.then_some(line)
            })
            .collect::<Vec<_>>()
            .join("\n")
            .trim()
            .to_owned();
        Some(Self {
            description,
            guidance,
        })
    }
}

#[allow(clippy::redundant_pub_crate)]
pub(crate) struct ProgressiveArgs {
    name: Expr,
    version: Expr,
    description: Option<Expr>,
    namespaces: Vec<ProgressiveNamespaceArg>,
    workflow_uri_template: LitStr,
    auth: AuthArgs,
}

impl syn::parse::Parse for ProgressiveArgs {
    fn parse(input: syn::parse::ParseStream<'_>) -> syn::Result<Self> {
        let metas = Punctuated::<Meta, Token![,]>::parse_terminated(input)?;
        let mut args = Self {
            name: syn::parse_quote!("mcp-progressive-server"),
            version: syn::parse_quote!("0"),
            description: None,
            namespaces: Vec::new(),
            workflow_uri_template: LitStr::new(
                "workflows://{workflow}",
                proc_macro2::Span::call_site(),
            ),
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
                Meta::NameValue(name_value)
                    if path_is(&name_value.path, "workflow_uri_template") =>
                {
                    let Expr::Lit(expr_lit) = name_value.value else {
                        return Err(syn::Error::new_spanned(
                            name_value,
                            "workflow_uri_template must be a string literal",
                        ));
                    };
                    let Lit::Str(template) = expr_lit.lit else {
                        return Err(syn::Error::new_spanned(
                            expr_lit,
                            "workflow_uri_template must be a string literal",
                        ));
                    };
                    args.workflow_uri_template = template;
                }
                Meta::List(list) if path_is(&list.path, "namespace") => {
                    args.namespaces
                        .push(ProgressiveNamespaceArg::from_tokens(list.tokens)?);
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

struct ProgressiveNamespaceArg {
    name: syn::Ident,
    description: Expr,
}

impl ProgressiveNamespaceArg {
    fn from_tokens(tokens: TokenStream2) -> syn::Result<Self> {
        let parser = |input: syn::parse::ParseStream<'_>| {
            let name = input.parse::<syn::Ident>()?;
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
            Ok(Self { name, description })
        };
        parser.parse2(tokens)
    }
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
