//! Kinds and the Ontology (ADR-0002).
//!
//! A Kind is a named class of Node or of Edge. Its properties are written in
//! a profile of JSON Schema 2020-12, and registering it compiles one jsonpath
//! rule per property, which the Store's writes evaluate in the database.

use std::collections::BTreeMap;

use serde_json::Value;

use crate::TenantId;

/// The root Kind for Nodes, which every Ontology has.
pub const ROOT_NODE_KIND: &str = "node";

/// The root Kind for Edges, which every Ontology has.
pub const ROOT_EDGE_KIND: &str = "edge";

/// Whether a Kind classes Nodes or Edges, and for an Edge Kind, which Node
/// Kinds it may join.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum KindOf {
    Node,
    /// An Edge Kind joins a Node of a `source` Kind to a Node of a `target`
    /// Kind. A Node of a Kind below one of these qualifies too.
    ///
    /// When registering, an empty list inherits the parent's, and each Kind
    /// named must be equal to or below one the parent allows. A Kind read
    /// back holds the lists in effect.
    Edge {
        source: Vec<String>,
        target: Vec<String>,
    },
}

/// A Kind to register in one Tenant's Ontology.
#[derive(Debug, Clone, PartialEq)]
pub struct NewKind {
    pub tenant: TenantId,
    /// A letter followed by at most 62 letters, digits or underscores. `node`
    /// and `edge` are the root Kinds and cannot be registered.
    pub name: String,
    pub of: KindOf,
    /// The Kind this one specializes, of the same sort. `None` is the root
    /// Kind, [`ROOT_NODE_KIND`] or [`ROOT_EDGE_KIND`].
    pub parent: Option<String>,
    /// The Kind's own properties, as a JSON Schema object:
    /// `{"type": "object", "properties": {...}, "required": [...]}`.
    ///
    /// Each property's schema may use only `type`, `properties`, `required`,
    /// `enum`, `const`, `items`, `minimum`, `maximum`, `exclusiveMinimum`,
    /// `exclusiveMaximum`, `minLength`, `maxLength`, `minItems`, `maxItems`,
    /// `anyOf`, and `format` limited to `date`, `date-time` and `uuid`.
    /// Registration refuses any other keyword. A child Kind cannot declare a
    /// property its ancestors declare.
    pub properties: Value,
    /// The properties this Kind adds to Search. It also inherits every mark
    /// of its ancestors: Node Kinds search the name and Aliases, and Edge
    /// Kinds the Sentence.
    pub search: Vec<String>,
}

/// A Kind as the Ontology holds it: flattened with its ancestors.
#[derive(Debug, Clone, PartialEq)]
pub struct Kind {
    pub tenant: TenantId,
    pub name: String,
    pub of: KindOf,
    /// `None` only for a root Kind.
    pub parent: Option<String>,
    /// Every ancestor, the root first and the parent last. Empty for a root
    /// Kind.
    pub ancestors: Vec<String>,
    /// The flattened property schema, the Kind's own properties with its
    /// ancestors': `{"type": "object", "properties": {...}, "required":
    /// [...]}`.
    pub properties: Value,
    /// One compiled jsonpath rule per property of the flattened Kind.
    /// `jsonb_path_match(value, rule)` is true exactly when the value
    /// satisfies that property's schema.
    pub rules: BTreeMap<String, String>,
    /// Everything that feeds Search, inherited marks first.
    pub search: Vec<String>,
}

impl Kind {
    pub fn is_root(&self) -> bool {
        self.parent.is_none()
    }
}
