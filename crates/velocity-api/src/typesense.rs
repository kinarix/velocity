//! Re-export shim — the actual Typesense client + spec helpers live in
//! the `velocity-typesense` crate (Phase 5d-2), shared with the
//! operator. This module exists only so existing `crate::typesense::*`
//! imports inside `velocity-api` continue to resolve.

pub use velocity_typesense::{
    collection_spec, cross_collection_name, cross_collection_spec, schema_collection_name,
    CollectionSpec, SearchParams, TsField, TypesenseClient, TypesenseError,
};
