use super::*;
use crate::routine::{children::{ChildPort, ChildScope, ServiceChildren}, context::ContextPort};

type Helpers = Arc<dyn Fn() -> Result<Arc<dyn HelperRunner>, PortError> + Send + Sync>;

#[derive(Clone)]
pub(super) struct ContextBindings<S: SessionStore + Clone + Send + Sync + 'static> {
    pub context: Arc<CoordinatorContext<S>>,
    pub helpers: Helpers,
    pub service: InvocationService,
    pub definitions: BTreeMap<String, Arc<Definition>>,
    pub principal: AuthenticatedPrincipal,
    pub capabilities: BTreeSet<String>,
    pub scope: Option<ChildScope>,
}
impl<S: SessionStore + Clone + Send + Sync + 'static> InvocationAdapterFactory
    for ContextBindings<S>
{
    fn bind(
        &self,
        definition: Arc<Definition>,
        _: SelectedEntrypoint,
        caller: Caller,
    ) -> PortFuture<AdapterBindings> {
        let helpers = (self.helpers)();
        let allowed = definition
            .envelope()
            .spec
            .capabilities
            .is_subset(&self.capabilities)
            && caller.principal == principal_id(&self.principal);
        let capabilities = definition.envelope().spec.capabilities.clone();
        Box::pin(async move {
            if !allowed {
                return Err(PortError::Denied);
            }
            Ok(AdapterBindings {
                principal: caller.principal,
                helpers: helpers?,
                tools: Arc::new(NoTools),
                capabilities,
                targets: BTreeMap::new(),
            })
        })
    }
    fn bind_context(
        &self,
        definition: Arc<Definition>,
        _: SelectedEntrypoint,
        _: Caller,
    ) -> PortFuture<Option<Arc<dyn ContextPort>>> {
        let context = Arc::new(self.context.with_original_ceiling(definition.envelope().spec.capabilities.contains("context.original")));
        Box::pin(async move { Ok(Some(context as Arc<dyn ContextPort>)) })
    }
    fn bind_children(
        &self,
        definition: Arc<Definition>,
        selected: SelectedEntrypoint,
        caller: Caller,
    ) -> PortFuture<Option<Arc<dyn ChildPort>>> {
        if !definition
            .envelope()
            .spec
            .capabilities
            .contains("children.call")
        {
            return Box::pin(async { Ok(None) });
        }
        let service = self.service.clone();
        let definitions = self.definitions.clone();
        let scope = self.scope.clone().map(Ok).unwrap_or_else(|| {
            ChildScope::root(
                self.context.snapshot_identity().ok_or(PortError::Denied)?,
                caller.principal,
                self.capabilities.clone(),
                format!("{}:{}", definition.envelope().name, selected.name),
                tokio::time::Instant::now()
                    + definition
                        .limits()
                        .deadline
                        .min(std::time::Duration::from_secs(30)),
            )
        });
        let bindings = self.clone();
        let principal = self.principal.clone();
        Box::pin(async move {
            let factory = super::super::lease::bind(
                Arc::new(RoutineAgentHostFactory {
                    bindings: Arc::new(bindings),
                }),
                principal,
                None,
            );
            Ok(Some(Arc::new(
                ServiceChildren::new(service, definitions, scope?)?.with_factory(factory),
            ) as Arc<dyn ChildPort>))
        })
    }
    fn child_bindings(&self, scope: ChildScope) -> PortFuture<Arc<dyn InvocationAdapterFactory>> {
        let mut child = self.clone();
        child.context = Arc::new(self.context.child_context());
        child.capabilities = scope.capabilities.clone();
        child.scope = Some(scope);
        Box::pin(async move { Ok(Arc::new(child) as Arc<dyn InvocationAdapterFactory>) })
    }
}

struct NoTools;
impl AuthorizedToolExecutor for NoTools {
    fn execute(
        &self,
        _: crate::agent_host::authorized_tools::AuthorizedToolRequest,
        _: CancellationToken,
    ) -> crate::agent_host::authorized_tools::AuthorizedToolFuture<waltr_agent::ToolResult> {
        Box::pin(async { Err(crate::agent_host::authorized_tools::AuthorizedToolError::Denied) })
    }
    fn schemas(
        &self,
        _: Vec<crate::agent_host::authorized_tools::ConcreteToolTarget>,
        _: CancellationToken,
    ) -> crate::agent_host::authorized_tools::AuthorizedToolFuture<Vec<waltr_agent::ToolDefinition>>
    {
        Box::pin(async { Err(crate::agent_host::authorized_tools::AuthorizedToolError::Denied) })
    }
    fn cleanup(&self) -> crate::agent_host::authorized_tools::AuthorizedToolFuture<()> {
        Box::pin(async { Ok(()) })
    }
}
