/*
 * Copyright Cedar Contributors
 *
 * Licensed under the Apache License, Version 2.0 (the "License");
 * you may not use this file except in compliance with the License.
 * You may obtain a copy of the License at
 *
 *      https://www.apache.org/licenses/LICENSE-2.0
 *
 * Unless required by applicable law or agreed to in writing, software
 * distributed under the License is distributed on an "AS IS" BASIS,
 * WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
 * See the License for the specific language governing permissions and
 * limitations under the License.
 */

//! Entity Manifest definition and static analysis.

use std::collections::HashMap;
use std::fmt::{Display, Formatter};

use cedar_policy_core::ast::{
    BinaryOp, EntityUID, Expr, ExprKind, Literal, PolicyID, PolicySet, RequestType, UnaryOp, Var,
};
use cedar_policy_core::entities::err::EntitiesError;
use cedar_policy_core::impl_diagnostic_from_source_loc_opt_field;
use cedar_policy_core::parser::Loc;
use miette::Diagnostic;
use serde::{Deserialize, Serialize};
use serde_with::serde_as;
use smol_str::SmolStr;
use thiserror::Error;

use crate::{
    typecheck::{PolicyCheck, Typechecker},
    types::{EntityRecordKind, Type},
    ValidationMode, ValidatorSchema,
};
use crate::{ValidationResult, Validator};

/// Data structure storing what data is needed
/// based on the the [`RequestType`].
/// For each request type, the [`EntityManifest`] stores
/// a [`RootAccessTrie`] of data to retrieve.
///
/// `T` represents an optional type annotation on each
/// node in the [`AccessTrie`].
// CAUTION: this type is publicly exported in `cedar-policy`.
// Don't make fields `pub`, don't make breaking changes, and use caution
// when adding public methods.
#[doc = include_str!("../experimental_warning.md")]
#[serde_as]
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct EntityManifest<T = ()>
where
    T: Clone,
{
    /// A map from request types to [`RootAccessTrie`]s.
    #[serde_as(as = "Vec<(_, _)>")]
    #[serde(bound(deserialize = "T: Default"))]
    per_action: HashMap<RequestType, RootAccessTrie<T>>,
}

/// A map of data fields to [`AccessTrie`]s.
/// The keys to this map form the edges in the access trie,
/// pointing to sub-tries.
// CAUTION: this type is publicly exported in `cedar-policy`.
// Don't make fields `pub`, don't make breaking changes, and use caution
// when adding public methods.
#[doc = include_str!("../experimental_warning.md")]
pub type Fields<T> = HashMap<SmolStr, Box<AccessTrie<T>>>;

/// The root of a data path or [`RootAccessTrie`].
// CAUTION: this type is publicly exported in `cedar-policy`.
// Don't make fields `pub`, don't make breaking changes, and use caution
// when adding public methods.
#[doc = include_str!("../experimental_warning.md")]
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Hash)]
#[serde(rename_all = "camelCase")]
pub enum EntityRoot {
    /// Literal entity ids
    Literal(EntityUID),
    /// A Cedar variable
    Var(Var),
}

impl Display for EntityRoot {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        match self {
            EntityRoot::Literal(l) => write!(f, "{l}"),
            EntityRoot::Var(v) => write!(f, "{v}"),
        }
    }
}

/// A [`RootAccessTrie`] is a trie describing a set of
/// data paths to retrieve. Each edge in the trie
/// is either a record or entity dereference.
///
/// If an entity or record field does not exist in the backing store,
/// it is safe to stop loading data at that point.
///
/// `T` represents an optional type annotation on each
/// node in the [`AccessTrie`].
// CAUTION: this type is publicly exported in `cedar-policy`.
// Don't make fields `pub`, don't make breaking changes, and use caution
// when adding public methods.
#[doc = include_str!("../experimental_warning.md")]
#[serde_as]
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RootAccessTrie<T = ()>
where
    T: Clone,
{
    /// The data that needs to be loaded, organized by root.
    #[serde_as(as = "Vec<(_, _)>")]
    #[serde(bound(deserialize = "T: Default"))]
    trie: HashMap<EntityRoot, AccessTrie<T>>,
}

/// A Trie representing a set of data paths to load,
/// starting implicitly from a Cedar value.
///
/// `T` represents an optional type annotation on each
/// node in the [`AccessTrie`].
///
// CAUTION: this type is publicly exported in `cedar-policy`.
// Don't make fields `pub`, don't make breaking changes, and use caution
// when adding public methods.
#[doc = include_str!("../experimental_warning.md")]
#[serde_as]
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AccessTrie<T = ()> {
    /// Child data of this entity slice.
    /// The keys are edges in the trie pointing to sub-trie values.
    #[serde_as(as = "Vec<(_, _)>")]
    children: Fields<T>,
    /// For entity types, this boolean may be `true`
    /// to signal that all the ancestors in the entity hierarchy
    /// are required (transitively).
    ancestors_required: bool,
    /// Optional data annotation, usually used for type information.
    #[serde(skip_serializing, skip_deserializing)]
    #[serde(bound(deserialize = "T: Default"))]
    data: T,
}

/// A data path that may end with requesting the parents of
/// an entity.
#[derive(Debug, Clone, PartialEq, Eq)]
struct AccessPath {
    /// The root variable that begins the data path
    pub root: EntityRoot,
    /// The path of fields of entities or structs
    pub path: Vec<SmolStr>,
    /// Request all the parents in the entity hierarchy of this entity.
    pub ancestors_required: bool,
}

/// Entity manifest computation does not handle the full
/// cedar language. In particular, the policies must follow the
/// following grammar:
/// ```text
/// <expr> = <datapath-expr>
///          <datapath-expr> in <expr>
///          <expr> + <expr>
///          if <expr> { <expr> } { <expr> }
///          ... all other cedar operators not mentioned by datapath-expr

/// <datapath-expr> = <datapath-expr>.<field>
///                   <datapath-expr> has <field>
///                   <variable>
///                   <entity literal>
/// ```
/// The `get_expr_path` function handles `datapath-expr` expressions.
/// This error message tells the user not to use certain operators
/// before accessing record or entity attributes, breaking this grammar.
// CAUTION: this type is publicly exported in `cedar-policy`.
// Don't make fields `pub`, don't make breaking changes, and use caution
// when adding public methods.
#[derive(Debug, Clone, Error, Hash, Eq, PartialEq)]
#[error("for policy `{policy_id}`, failed to analyze expression while computing entity manifest`")]
pub struct FailedAnalysisError {
    /// Source location
    source_loc: Option<Loc>,
    /// Policy ID where the error occurred
    policy_id: PolicyID,
    /// The kind of the expression that was unexpected
    expr_kind: ExprKind<Option<Type>>,
}

impl Diagnostic for FailedAnalysisError {
    impl_diagnostic_from_source_loc_opt_field!(source_loc);

    fn help<'a>(&'a self) -> Option<Box<dyn Display + 'a>> {
        Some(Box::new(format!(
            "failed to compute entity manifest: {} operators are not allowed before accessing record or entity attributes",
            self.expr_kind.operator_description()
        )))
    }
}

/// Error when expressions are partial during entity
/// manifest computation
// CAUTION: this type is publicly exported in `cedar-policy`.
// Don't make fields `pub`, don't make breaking changes, and use caution
// when adding public methods.
#[derive(Debug, Clone, Error, Hash, Eq, PartialEq)]
#[error("entity slicing requires fully concrete policies. Got a policy with an unknown expression")]
pub struct PartialExpressionError {}

impl Diagnostic for PartialExpressionError {}

/// Error when the request is partial during entity
/// manifest computation
// CAUTION: this type is publicly exported in `cedar-policy`.
// Don't make fields `pub`, don't make breaking changes, and use caution
// when adding public methods.
#[derive(Debug, Clone, Error, Hash, Eq, PartialEq)]
#[error("entity slicing requires a fully concrete request. Got a partial request")]
pub struct PartialRequestError {}
impl Diagnostic for PartialRequestError {}

/// An error generated by entity slicing.
/// See [`FailedAnalysisError`] for details on the fragment
/// of Cedar handled by entity slicing.
#[derive(Debug, Error)]
pub enum EntityManifestError {
    /// A validation error was encountered
    // TODO (#1158) impl Error for ValidationResult (it already is implemented for api::ValidationResult)
    #[error("a validation error occurred")]
    Validation(ValidationResult),
    /// A entities error was encountered
    #[error(transparent)]
    Entities(#[from] EntitiesError),

    /// The request was partial
    #[error(transparent)]
    PartialRequest(#[from] PartialRequestError),
    /// A policy was partial
    #[error(transparent)]
    PartialExpression(#[from] PartialExpressionError),

    /// A policy was not analyzable because it used unsupported operators
    /// before a [`ExprKind::GetAttr`]
    /// See [`FailedAnalysisError`] for more details.
    #[error(transparent)]
    FailedAnalysis(#[from] FailedAnalysisError),
}

impl<T: Clone> EntityManifest<T> {
    /// Get the contents of the entity manifest
    /// indexed by the type of the request.
    pub fn per_action(&self) -> &HashMap<RequestType, RootAccessTrie<T>> {
        &self.per_action
    }
}

/// Union two tries by combining the fields.
fn union_fields<T: Clone>(first: &Fields<T>, second: &Fields<T>) -> Fields<T> {
    let mut res = first.clone();
    for (key, value) in second {
        res.entry(key.clone())
            .and_modify(|existing| *existing = Box::new((*existing).union(value)))
            .or_insert(value.clone());
    }
    res
}

impl AccessPath {
    /// Convert a [`AccessPath`] into corresponding [`RootAccessTrie`].
    fn to_root_access_trie(&self) -> RootAccessTrie {
        self.to_root_access_trie_with_leaf(AccessTrie {
            ancestors_required: true,
            children: Default::default(),
            data: (),
        })
    }

    /// Convert an [`AccessPath`] to a [`RootAccessTrie`], and also
    /// add a full trie as the leaf at the end.
    fn to_root_access_trie_with_leaf(&self, leaf_trie: AccessTrie) -> RootAccessTrie {
        let mut current = leaf_trie;
        // reverse the path, visiting the last access first
        for field in self.path.iter().rev() {
            let mut fields = HashMap::new();
            fields.insert(field.clone(), Box::new(current));
            current = AccessTrie {
                ancestors_required: false,
                children: fields,
                data: (),
            };
        }

        let mut primary_map = HashMap::new();
        primary_map.insert(self.root.clone(), current);
        RootAccessTrie { trie: primary_map }
    }
}

impl<T: Clone> RootAccessTrie<T> {
    /// Get the trie as a hash map from [`EntityRoot`]
    /// to sub-[`AccessTrie`]s.
    pub fn trie(&self) -> &HashMap<EntityRoot, AccessTrie<T>> {
        &self.trie
    }
}

impl RootAccessTrie {
    /// Create an empty [`RootAccessTrie`] that requests nothing.
    pub fn new() -> Self {
        Self {
            trie: Default::default(),
        }
    }
}

impl<T: Clone> RootAccessTrie<T> {
    /// Union two [`RootAccessTrie`]s together.
    /// The new trie requests the data from both of the original.
    fn union(&self, other: &Self) -> Self {
        let mut res = self.clone();
        for (key, value) in &other.trie {
            res.trie
                .entry(key.clone())
                .and_modify(|existing| *existing = (*existing).union(value))
                .or_insert(value.clone());
        }
        res
    }
}

impl Default for RootAccessTrie {
    fn default() -> Self {
        Self::new()
    }
}

impl<T: Clone> AccessTrie<T> {
    /// Union two [`AccessTrie`]s together.
    /// The new trie requests the data from both of the original.
    fn union(&self, other: &Self) -> Self {
        Self {
            children: union_fields(&self.children, &other.children),
            ancestors_required: self.ancestors_required || other.ancestors_required,
            data: self.data.clone(),
        }
    }

    /// Get the children of this [`AccessTrie`].
    pub fn children(&self) -> &Fields<T> {
        &self.children
    }

    /// Get a boolean which is true if this trie
    /// requires all ancestors of the entity to be loaded.
    pub fn ancestors_required(&self) -> bool {
        self.ancestors_required
    }

    /// Get the data associated with this [`AccessTrie`].
    /// This is usually `()` unless it is annotated by a type.
    pub fn data(&self) -> &T {
        &self.data
    }
}

impl AccessTrie {
    /// A new trie that requests no data.
    fn new() -> Self {
        Self {
            children: Default::default(),
            ancestors_required: false,
            data: (),
        }
    }
}

/// Computes an [`EntityManifest`] from the schema and policies.
/// The policies must validate against the schema in strict mode,
/// otherwise an error is returned.
pub fn compute_entity_manifest(
    schema: &ValidatorSchema,
    policies: &PolicySet,
) -> Result<EntityManifest, EntityManifestError> {
    // first, run strict validation to ensure there are no errors
    let validator = Validator::new(schema.clone());
    let validation_res = validator.validate(policies, ValidationMode::Strict);
    if !validation_res.validation_passed() {
        return Err(EntityManifestError::Validation(validation_res));
    }

    let mut manifest: HashMap<RequestType, RootAccessTrie> = HashMap::new();

    // now, for each policy we add the data it requires to the manifest
    for policy in policies.policies() {
        // typecheck the policy and get all the request environments
        let typechecker = Typechecker::new(schema, ValidationMode::Strict, policy.id().clone());
        let request_envs = typechecker.typecheck_by_request_env(policy.template());
        for (request_env, policy_check) in request_envs {
            let new_primary_slice = match policy_check {
                PolicyCheck::Success(typechecked_expr) => {
                    // compute the trie from the typechecked expr
                    // using static analysis
                    compute_root_trie(&typechecked_expr, policy.id())
                }
                PolicyCheck::Irrelevant(_, _) => {
                    // this policy is irrelevant, so we need no data
                    Ok(RootAccessTrie::new())
                }

                // PANIC SAFETY: policy check should not fail after full strict validation above.
                #[allow(clippy::panic)]
                PolicyCheck::Fail(_errors) => {
                    panic!("Policy check failed after validation succeeded")
                }
            }?;

            let request_type = request_env
                .to_request_type()
                .ok_or(PartialRequestError {})?;
            // Add to the manifest based on the request type.
            manifest
                .entry(request_type)
                .and_modify(|existing| {
                    *existing = existing.union(&new_primary_slice);
                })
                .or_insert(new_primary_slice);
        }
    }

    Ok(EntityManifest {
        per_action: manifest,
    })
}

/// A static analysis on type-annotated cedar expressions.
/// Computes the [`RootAccessTrie`] representing all the data required
/// to evaluate the expression.
fn compute_root_trie(
    expr: &Expr<Option<Type>>,
    policy_id: &PolicyID,
) -> Result<RootAccessTrie, EntityManifestError> {
    let mut primary_slice = RootAccessTrie::new();
    add_to_root_trie(&mut primary_slice, expr, policy_id, false)?;
    Ok(primary_slice)
}

/// Add the expression's requested data to the [`RootAccessTrie`].
/// This handles <expr>s from the grammar (see [`FailedAnalysisError`])
/// while [`get_expr_path`] handles the <datapath-expr>s.
fn add_to_root_trie(
    root_trie: &mut RootAccessTrie,
    expr: &Expr<Option<Type>>,
    policy_id: &PolicyID,
    should_load_all: bool,
) -> Result<(), EntityManifestError> {
    match expr.expr_kind() {
        // Literals, variables, and unkonwns without any GetAttr operations
        // on them are okay, since no fields need to be loaded.
        ExprKind::Lit(_) => Ok(()),
        ExprKind::Var(_) => Ok(()),
        ExprKind::Slot(_) => Ok(()),
        ExprKind::Unknown(_) => Err(PartialExpressionError {})?,
        ExprKind::If {
            test_expr,
            then_expr,
            else_expr,
        } => {
            add_to_root_trie(root_trie, test_expr, policy_id, should_load_all)?;
            add_to_root_trie(root_trie, then_expr, policy_id, should_load_all)?;
            add_to_root_trie(root_trie, else_expr, policy_id, should_load_all)?;
            Ok(())
        }
        ExprKind::And { left, right } => {
            add_to_root_trie(root_trie, left, policy_id, should_load_all)?;
            add_to_root_trie(root_trie, right, policy_id, should_load_all)?;
            Ok(())
        }
        ExprKind::Or { left, right } => {
            add_to_root_trie(root_trie, left, policy_id, should_load_all)?;
            add_to_root_trie(root_trie, right, policy_id, should_load_all)?;
            Ok(())
        }
        ExprKind::UnaryApp { op, arg } => {
            match op {
                UnaryOp::Not => add_to_root_trie(root_trie, arg, policy_id, should_load_all)?,
                UnaryOp::Neg => add_to_root_trie(root_trie, arg, policy_id, should_load_all)?,
            };
            Ok(())
        }
        ExprKind::BinaryApp { op, arg1, arg2 } => match op {
            // Special case! Equality between records requires
            // that we load all fields.
            // This could be made more precise if we check the type.
            BinaryOp::Eq => {
                add_to_root_trie(root_trie, arg1, policy_id, true)?;
                add_to_root_trie(root_trie, arg2, policy_id, true)?;
                Ok(())
            }
            BinaryOp::In => {
                // Recur normally on the rhs
                add_to_root_trie(root_trie, arg2, policy_id, should_load_all)?;

                // The lhs should be a datapath expression.
                let mut flat_slice = get_expr_path(arg1, policy_id)?;
                flat_slice.ancestors_required = true;
                *root_trie = root_trie.union(&flat_slice.to_root_access_trie());
                Ok(())
            }
            BinaryOp::Contains | BinaryOp::ContainsAll | BinaryOp::ContainsAny => {
                // Like equality, another special case for records.
                add_to_root_trie(root_trie, arg1, policy_id, true)?;
                add_to_root_trie(root_trie, arg2, policy_id, true)?;
                Ok(())
            }
            BinaryOp::Less | BinaryOp::LessEq | BinaryOp::Add | BinaryOp::Sub | BinaryOp::Mul => {
                // These operators work on literals, so no special
                // case is needed.
                add_to_root_trie(root_trie, arg1, policy_id, should_load_all)?;
                add_to_root_trie(root_trie, arg2, policy_id, should_load_all)?;
                Ok(())
            }
            BinaryOp::GetTag | BinaryOp::HasTag => {
                unimplemented!("interaction between RFCs 74 and 82")
            }
        },
        ExprKind::ExtensionFunctionApp { fn_name: _, args } => {
            // WARNING: this code assumes that extension functions
            // don't take full structs as inputs.
            // If they did, we would need to use logic similar to the Eq binary operator.
            for arg in args.iter() {
                add_to_root_trie(root_trie, arg, policy_id, should_load_all)?;
            }
            Ok(())
        }
        ExprKind::Like { expr, pattern: _ } => {
            add_to_root_trie(root_trie, expr, policy_id, should_load_all)?;
            Ok(())
        }
        ExprKind::Is {
            expr,
            entity_type: _,
        } => {
            add_to_root_trie(root_trie, expr, policy_id, should_load_all)?;
            Ok(())
        }
        ExprKind::Set(contents) => {
            for expr in &**contents {
                add_to_root_trie(root_trie, expr, policy_id, should_load_all)?;
            }
            Ok(())
        }
        ExprKind::Record(content) => {
            for expr in content.values() {
                add_to_root_trie(root_trie, expr, policy_id, should_load_all)?;
            }
            Ok(())
        }
        ExprKind::HasAttr { expr, attr } => {
            let mut flat_slice = get_expr_path(expr, policy_id)?;
            flat_slice.path.push(attr.clone());
            *root_trie = root_trie.union(&flat_slice.to_root_access_trie());
            Ok(())
        }
        ExprKind::GetAttr { .. } => {
            let flat_slice = get_expr_path(expr, policy_id)?;

            // PANIC SAFETY: Successfuly typechecked expressions should always have annotated types.
            #[allow(clippy::expect_used)]
            let leaf_field = if should_load_all {
                type_to_access_trie(
                    expr.data()
                        .as_ref()
                        .expect("Typechecked expression missing type"),
                )
            } else {
                AccessTrie::new()
            };

            *root_trie = root_trie.union(&flat_slice.to_root_access_trie_with_leaf(leaf_field));
            Ok(())
        }
    }
}

/// Compute the full [`AccessTrie`] required for the type.
fn type_to_access_trie(ty: &Type) -> AccessTrie {
    match ty {
        // if it's not an entity or record, slice ends here
        Type::ExtensionType { .. }
        | Type::Never
        | Type::True
        | Type::False
        | Type::Primitive { .. }
        | Type::Set { .. } => AccessTrie::new(),
        Type::EntityOrRecord(record_type) => entity_or_record_to_access_trie(record_type),
    }
}

/// Compute the full [`AccessTrie`] for the given entity or record type.
fn entity_or_record_to_access_trie(ty: &EntityRecordKind) -> AccessTrie {
    match ty {
        EntityRecordKind::ActionEntity { attrs, .. } | EntityRecordKind::Record { attrs, .. } => {
            let mut fields = HashMap::new();
            for (attr_name, attr_type) in attrs.iter() {
                fields.insert(
                    attr_name.clone(),
                    Box::new(type_to_access_trie(&attr_type.attr_type)),
                );
            }
            AccessTrie {
                children: fields,
                ancestors_required: false,
                data: (),
            }
        }

        EntityRecordKind::Entity(_) | EntityRecordKind::AnyEntity => {
            // no need to load data for entities, which are compared
            // using ids
            AccessTrie::new()
        }
    }
}

/// Given an expression, get the corresponding data path
/// starting with a variable.
/// If the expression is not a `<datapath-expr>`, return an error.
/// See [`FailedAnalysisError`] for more information.
fn get_expr_path(
    expr: &Expr<Option<Type>>,
    policy_id: &PolicyID,
) -> Result<AccessPath, EntityManifestError> {
    Ok(match expr.expr_kind() {
        ExprKind::Slot(slot_id) => {
            if slot_id.is_principal() {
                AccessPath {
                    root: EntityRoot::Var(Var::Principal),
                    path: vec![],
                    ancestors_required: false,
                }
            } else {
                assert!(slot_id.is_resource());
                AccessPath {
                    root: EntityRoot::Var(Var::Resource),
                    path: vec![],
                    ancestors_required: false,
                }
            }
        }
        ExprKind::Var(var) => AccessPath {
            root: EntityRoot::Var(*var),
            path: vec![],
            ancestors_required: false,
        },
        ExprKind::GetAttr { expr, attr } => {
            let mut slice = get_expr_path(expr, policy_id)?;
            slice.path.push(attr.clone());
            slice
        }
        ExprKind::Lit(Literal::EntityUID(literal)) => AccessPath {
            root: EntityRoot::Literal((**literal).clone()),
            path: vec![],
            ancestors_required: false,
        },
        ExprKind::Unknown(_) => Err(PartialExpressionError {})?,
        // all other variants of expressions result in failure to analyze.
        _ => Err(EntityManifestError::FailedAnalysis(FailedAnalysisError {
            source_loc: expr.source_loc().cloned(),
            policy_id: policy_id.clone(),
            expr_kind: expr.expr_kind().clone(),
        }))?,
    })
}

#[cfg(test)]
mod entity_slice_tests {
    use cedar_policy_core::{ast::PolicyID, extensions::Extensions, parser::parse_policy};

    use super::*;

    // Schema for testing in this module
    fn schema() -> ValidatorSchema {
        ValidatorSchema::from_cedarschema_str(
            "
entity User = {
  name: String,
};

entity Document;

action Read appliesTo {
  principal: [User],
  resource: [Document]
};
    ",
            Extensions::all_available(),
        )
        .unwrap()
        .0
    }

    #[test]
    fn test_simple_entity_manifest() {
        let mut pset = PolicySet::new();
        let policy = parse_policy(
            None,
            "permit(principal, action, resource)
when {
    principal.name == \"John\"
};",
        )
        .expect("should succeed");
        pset.add(policy.into()).expect("should succeed");

        let schema = schema();

        let entity_manifest = compute_entity_manifest(&schema, &pset).expect("Should succeed");
        let expected = serde_json::json! ({
          "perAction": [
            [
              {
                "principal": "User",
                "action": {
                  "ty": "Action",
                  "eid": "Read"
                },
                "resource": "Document"
              },
              {
                "trie": [
                  [
                    {
                      "var": "principal"
                    },
                    {
                      "children": [
                        [
                          "name",
                          {
                            "children": [],
                            "ancestorsRequired": false
                          }
                        ]
                      ],
                      "ancestorsRequired": false
                    }
                  ]
                ]
              }
            ]
          ]
        });
        let expected_manifest = serde_json::from_value(expected).unwrap();
        assert_eq!(entity_manifest, expected_manifest);
    }

    #[test]
    fn test_empty_entity_manifest() {
        let mut pset = PolicySet::new();
        let policy =
            parse_policy(None, "permit(principal, action, resource);").expect("should succeed");
        pset.add(policy.into()).expect("should succeed");

        let schema = schema();

        let entity_manifest = compute_entity_manifest(&schema, &pset).expect("Should succeed");
        let expected = serde_json::json!(
        {
          "perAction": [
            [
              {
                "principal": "User",
                "action": {
                  "ty": "Action",
                  "eid": "Read"
                },
                "resource": "Document"
              },
              {
                "trie": [
                ]
              }
            ]
          ]
        });
        let expected_manifest = serde_json::from_value(expected).unwrap();
        assert_eq!(entity_manifest, expected_manifest);
    }

    #[test]
    fn test_entity_manifest_ancestors_required() {
        let mut pset = PolicySet::new();
        let policy = parse_policy(
            None,
            "permit(principal, action, resource)
when {
    principal in resource || principal.manager in resource
};",
        )
        .expect("should succeed");
        pset.add(policy.into()).expect("should succeed");

        let schema = ValidatorSchema::from_cedarschema_str(
            "
entity User in [Document] = {
  name: String,
  manager: User
};
entity Document;
action Read appliesTo {
  principal: [User],
  resource: [Document]
};
  ",
            Extensions::all_available(),
        )
        .unwrap()
        .0;

        let entity_manifest = compute_entity_manifest(&schema, &pset).expect("Should succeed");
        let expected = serde_json::json!(
        {
          "perAction": [
            [
              {
                "principal": "User",
                "action": {
                  "ty": "Action",
                  "eid": "Read"
                },
                "resource": "Document"
              },
              {
                "trie": [
                  [
                    {
                      "var": "principal"
                    },
                    {
                      "children": [
                        [
                          "manager",
                          {
                            "children": [],
                            "ancestorsRequired": true
                          }
                        ]
                      ],
                      "ancestorsRequired": true
                    }
                  ]
                ]
              }
            ]
          ]
        });
        let expected_manifest = serde_json::from_value(expected).unwrap();
        assert_eq!(entity_manifest, expected_manifest);
    }

    #[test]
    fn test_entity_manifest_multiple_types() {
        let mut pset = PolicySet::new();
        let policy = parse_policy(
            None,
            "permit(principal, action, resource)
when {
    principal.name == \"John\"
};",
        )
        .expect("should succeed");
        pset.add(policy.into()).expect("should succeed");

        let schema = ValidatorSchema::from_cedarschema_str(
            "
entity User = {
  name: String,
};

entity OtherUserType = {
  name: String,
  irrelevant: String,
};

entity Document;

action Read appliesTo {
  principal: [User, OtherUserType],
  resource: [Document]
};
        ",
            Extensions::all_available(),
        )
        .unwrap()
        .0;

        let entity_manifest = compute_entity_manifest(&schema, &pset).expect("Should succeed");
        let expected = serde_json::json!(
        {
          "perAction": [
            [
              {
                "principal": "User",
                "action": {
                  "ty": "Action",
                  "eid": "Read"
                },
                "resource": "Document"
              },
              {
                "trie": [
                  [
                    {
                      "var": "principal"
                    },
                    {
                      "children": [
                        [
                          "name",
                          {
                            "children": [],
                            "ancestorsRequired": false
                          }
                        ]
                      ],
                      "ancestorsRequired": false
                    }
                  ]
                ]
              }
            ],
            [
              {
                "principal": "OtherUserType",
                "action": {
                  "ty": "Action",
                  "eid": "Read"
                },
                "resource": "Document"
              },
              {
                "trie": [
                  [
                    {
                      "var": "principal"
                    },
                    {
                      "children": [
                        [
                          "name",
                          {
                            "children": [],
                            "ancestorsRequired": false
                          }
                        ]
                      ],
                      "ancestorsRequired": false
                    }
                  ]
                ]
              }
            ]
          ]
            });
        let expected_manifest = serde_json::from_value(expected).unwrap();
        assert_eq!(entity_manifest, expected_manifest);
    }

    #[test]
    fn test_entity_manifest_multiple_branches() {
        let mut pset = PolicySet::new();
        let policy1 = parse_policy(
            None,
            r#"
permit(
  principal,
  action == Action::"Read",
  resource
)
when
{
  resource.readers.contains(principal)
};"#,
        )
        .unwrap();
        let policy2 = parse_policy(
            Some(PolicyID::from_string("Policy2")),
            r#"permit(
  principal,
  action == Action::"Read",
  resource
)
when
{
  resource.metadata.owner == principal
};"#,
        )
        .unwrap();
        pset.add(policy1.into()).expect("should succeed");
        pset.add(policy2.into()).expect("should succeed");

        let schema = ValidatorSchema::from_cedarschema_str(
            "
entity User;

entity Metadata = {
   owner: User,
   time: String,
};

entity Document = {
  metadata: Metadata,
  readers: Set<User>,
};

action Read appliesTo {
  principal: [User],
  resource: [Document]
};
        ",
            Extensions::all_available(),
        )
        .unwrap()
        .0;

        let entity_manifest = compute_entity_manifest(&schema, &pset).expect("Should succeed");
        let expected = serde_json::json!(
        {
          "perAction": [
            [
              {
                "principal": "User",
                "action": {
                  "ty": "Action",
                  "eid": "Read"
                },
                "resource": "Document"
              },
              {
                "trie": [
                  [
                    {
                      "var": "resource"
                    },
                    {
                      "children": [
                        [
                          "metadata",
                          {
                            "children": [
                              [
                                "owner",
                                {
                                  "children": [],
                                  "ancestorsRequired": false
                                }
                              ]
                            ],
                            "ancestorsRequired": false
                          }
                        ],
                        [
                          "readers",
                          {
                            "children": [],
                            "ancestorsRequired": false
                          }
                        ]
                      ],
                      "ancestorsRequired": false
                    }
                  ]
                ]
              }
            ]
          ]
        });
        let expected_manifest = serde_json::from_value(expected).unwrap();
        assert_eq!(entity_manifest, expected_manifest);
    }

    #[test]
    fn test_entity_manifest_struct_equality() {
        let mut pset = PolicySet::new();
        // we need to load all of the metadata, not just nickname
        // no need to load actual name
        let policy = parse_policy(
            None,
            r#"permit(principal, action, resource)
when {
    principal.metadata.nickname == "timmy" && principal.metadata == {
        "friends": [ "oliver" ],
        "nickname": "timmy"
    }
};"#,
        )
        .expect("should succeed");
        pset.add(policy.into()).expect("should succeed");

        let schema = ValidatorSchema::from_cedarschema_str(
            "
entity User = {
  name: String,
  metadata: {
    friends: Set<String>,
    nickname: String,
  },
};

entity Document;

action BeSad appliesTo {
  principal: [User],
  resource: [Document]
};
        ",
            Extensions::all_available(),
        )
        .unwrap()
        .0;

        let entity_manifest = compute_entity_manifest(&schema, &pset).expect("Should succeed");
        let expected = serde_json::json!(
        {
          "perAction": [
            [
              {
                "principal": "User",
                "action": {
                  "ty": "Action",
                  "eid": "BeSad"
                },
                "resource": "Document"
              },
              {
                "trie": [
                  [
                    {
                      "var": "principal"
                    },
                    {
                      "children": [
                        [
                          "metadata",
                          {
                            "children": [
                              [
                                "nickname",
                                {
                                  "children": [],
                                  "ancestorsRequired": false
                                }
                              ],
                              [
                                "friends",
                                {
                                  "children": [],
                                  "ancestorsRequired": false
                                }
                              ]
                            ],
                            "ancestorsRequired": false
                          }
                        ]
                      ],
                      "ancestorsRequired": false
                    }
                  ]
                ]
              }
            ]
          ]
        });
        let expected_manifest = serde_json::from_value(expected).unwrap();
        assert_eq!(entity_manifest, expected_manifest);
    }

    #[test]
    fn test_entity_manifest_struct_equality_left_right_different() {
        let mut pset = PolicySet::new();
        // we need to load all of the metadata, not just nickname
        // no need to load actual name
        let policy = parse_policy(
            None,
            r#"permit(principal, action, resource)
when {
    principal.metadata == resource.metadata
};"#,
        )
        .expect("should succeed");
        pset.add(policy.into()).expect("should succeed");

        let schema = ValidatorSchema::from_cedarschema_str(
            "
entity User = {
  name: String,
  metadata: {
    friends: Set<String>,
    nickname: String,
  },
};

entity Document;

action Hello appliesTo {
  principal: [User],
  resource: [User]
};
        ",
            Extensions::all_available(),
        )
        .unwrap()
        .0;

        let entity_manifest = compute_entity_manifest(&schema, &pset).expect("Should succeed");
        let expected = serde_json::json!(
        {
          "perAction": [
            [
              {
                "principal": "User",
                "action": {
                  "ty": "Action",
                  "eid": "Hello"
                },
                "resource": "User"
              },
              {
                "trie": [
                  [
                    {
                      "var": "resource"
                    },
                    {
                      "children": [
                        [
                          "metadata",
                          {
                            "children": [
                              [
                                "friends",
                                {
                                  "children": [],
                                  "ancestorsRequired": false
                                }
                              ],
                              [
                                "nickname",
                                {
                                  "children": [],
                                  "ancestorsRequired": false
                                }
                              ]
                            ],
                            "ancestorsRequired": false
                          }
                        ]
                      ],
                      "ancestorsRequired": false
                    }
                  ],
                  [
                    {
                      "var": "principal"
                    },
                    {
                      "children": [
                        [
                          "metadata",
                          {
                            "children": [
                              [
                                "nickname",
                                {
                                  "children": [],
                                  "ancestorsRequired": false
                                }
                              ],
                              [
                                "friends",
                                {
                                  "children": [],
                                  "ancestorsRequired": false
                                }
                              ]
                            ],
                            "ancestorsRequired": false
                          }
                        ]
                      ],
                      "ancestorsRequired": false
                    }
                  ]
                ]
              }
            ]
          ]
        });
        let expected_manifest = serde_json::from_value(expected).unwrap();
        assert_eq!(entity_manifest, expected_manifest);
    }
}
