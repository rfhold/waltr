//! Model catalog reducer responsibilities.

use crate::{CatalogEffect, CatalogEvent, ClientEffect, ClientState, ClientUpdate};

pub fn reduce(state: &mut ClientState, event: CatalogEvent) -> ClientUpdate {
    match event {
        CatalogEvent::ModelsSearchRequested {
            request_id,
            query,
            provider_id,
            limit,
        } => {
            state.set_catalog_model_search_loading(request_id.clone(), query.clone());
            ClientUpdate::with_effect(ClientEffect::Catalog(CatalogEffect::SearchModels {
                request_id,
                query,
                provider_id,
                limit,
            }))
        }
        CatalogEvent::ModelsSearched {
            request_id,
            models,
            stale,
        } => {
            state.set_catalog_model_search_ready(&request_id, models, stale);
            ClientUpdate::default()
        }
        CatalogEvent::ModelsSearchFailed { request_id, error } => {
            state.set_catalog_model_search_failed(&request_id, error.to_string());
            ClientUpdate::default()
        }
        CatalogEvent::MatchesSuggestRequested {
            request_id,
            provider_key,
            model_key,
            limit,
        } => {
            state.set_catalog_match_suggestion_loading(request_id.clone());
            ClientUpdate::with_effect(ClientEffect::Catalog(CatalogEffect::SuggestMatches {
                request_id,
                provider_key,
                model_key,
                limit,
            }))
        }
        CatalogEvent::MatchesSuggested {
            request_id,
            matches,
            stale,
        } => {
            state.set_catalog_match_suggestion_ready(&request_id, matches, stale);
            ClientUpdate::default()
        }
        CatalogEvent::MatchesSuggestFailed { request_id, error } => {
            state.set_catalog_match_suggestion_failed(&request_id, error.to_string());
            ClientUpdate::default()
        }
    }
}
