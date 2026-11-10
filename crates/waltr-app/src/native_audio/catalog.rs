//! Current shared-runtime discovery, with operation-specific invalidation.
use waltr_client::{ClientSnapshot, SessionDictationView};
use waltr_desktop::interaction::Invocation;

#[derive(Clone)]
pub(crate) struct Catalog {
    definitions: waltr_client::RoutineDefinitionSnapshot,
    dictation: Option<SessionDictationView>,
    sessions: Vec<String>,
}

pub(crate) struct Pin {
    invocation: Invocation,
    dictation: Option<SessionDictationView>,
}
impl Catalog {
    pub fn new(snapshot: &ClientSnapshot) -> Self {
        Self {
            definitions: snapshot.routines.definitions.clone(),
            dictation: snapshot
                .config
                .current
                .as_ref()
                .and_then(|c| c.session_dictation.clone()),
            sessions: snapshot
                .sessions
                .current
                .iter()
                .map(|s| s.id.clone())
                .collect(),
        }
    }
    pub fn resolve(
        &self,
        routine: &str,
        entrypoint: &str,
        source: Option<String>,
        correlation: String,
    ) -> Result<Invocation, String> {
        let definition = self
            .definitions
            .definitions
            .iter()
            .find(|d| d.routine_name == routine)
            .ok_or("Routine definition is not available")?;
        if source.as_ref().is_some_and(|s| !self.sessions.contains(s)) {
            return Err("Source session is not available".into());
        }
        let parameters = waltr_desktop::interaction::default_parameters(&definition.parameters)
            .map_err(|_| "Routine parameters require explicit values")?;
        Invocation::new(
            definition.clone(),
            self.definitions
                .limits
                .clone()
                .ok_or("Routine limits are not available")?
                .into(),
            entrypoint.into(),
            parameters,
            correlation,
            source,
        )
        .map_err(|_| "Routine selection is not supported".into())
    }
    pub fn pin(&self, invocation: Invocation, chat: bool) -> Pin {
        Pin {
            invocation,
            dictation: chat.then(|| self.dictation.clone()).flatten(),
        }
    }
    pub fn current(&self, pin: &Pin) -> bool {
        let selection = &pin.invocation.selection;
        self.definitions
            .definitions
            .iter()
            .any(|d| d == &pin.invocation.limits.definition)
            && self.definitions.limits.clone().is_some_and(|l| {
                waltr_client::interaction::wire::ServerLimits::from(l)
                    == pin.invocation.limits.server
            })
            && selection
                .source_session
                .as_ref()
                .is_none_or(|s| self.sessions.contains(s))
            && pin
                .dictation
                .as_ref()
                .is_none_or(|d| self.dictation.as_ref() == Some(d))
    }
    pub fn matches_chat(&self, routine: &str, trigger: &str, input: &str, output: &str) -> bool {
        self.dictation.as_ref().is_some_and(|d| {
            d.routine == routine && d.trigger == trigger && d.input == input && d.output == output
        })
    }
}

#[cfg(test)]
#[path = "catalog_tests.rs"]
mod tests;
