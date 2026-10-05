//! Host-owned isolated Apps container, assets, and listener policy.

mod assets;
mod container;

pub use container::{
    AppsContainer, AppsContainerConfig, SpawnedAppsContainer, spawn_app_container,
};
