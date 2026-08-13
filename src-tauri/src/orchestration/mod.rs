//! Nestra Provider Orchestration Layer — the routing/work control plane.
//!
//! This module owns the **routing/work identity model** and the persistence
//! layer for everything the local gateway observes and decides.
//!
//! # Layers
//!
//! - [`identity`] — the canonical Nestra identity hierarchy
//!   (`Agent → LogicalSession → AgentRun/ChildSession → Task → Request`) and
//!   the routing types (`TaskContext`, `ResolvedRoute`/`RouteRecord`,
//!   `CredentialHandle`). The credential boundary is enforced here and in
//!   [`store`].
//! - [`store`] — CRUD over the orchestration tables (`routing_policy`,
//!   `logical_session`, `run`, `task`, `route_request`, `route_migration`,
//!   `model_catalog`). Persisted structs are credential-free by construction.
//! - [`router`] — the route resolver (explicit → affinity → capability → fail
//!   closed).
//! - [`migration`] — the 6-class failure taxonomy and migration decision
//!   engine.
//! - [`health`] / [`quota_state`] — rolling health and reactive quota stores.
//! - [`capability_registry`] — the merged model-ability index the router
//!   consults.
//! - [`cache`] — prompt-cache breakpoint planner (Anthropic).
//! - [`gateway`] — the loopback HTTP server, protocol handlers, and stream
//!   passthrough.

pub mod cache;
pub mod capability_registry;
pub mod gateway;
pub mod health;
pub mod identity;
pub mod migration;
pub mod quota_state;
pub mod router;
pub mod store;

pub use identity::{
    CapabilityReq, CacheStrategy, CredentialHandle, NativeTaskRef, ResolvedRoute, RoleSource,
    RouteReason, RouteRecord, SubagentRole, TaskContext, TaskLifecycle,
};
