//! Promise kinds (ADR 0064 declared kinds, #3587 layer 5).
//!
//! A promise kind is data: three rules (durable, ready and kept), each a list
//! of ADR 0066 condition templates over the submission, `$cr` for a change
//! request or `$artifact` for an artifact. The conditions a rule may use are
//! code: [`VOCABULARY`] admits each `<variable> <path> <operator> <literal>`
//! shape only where a real observation ([`Observation`]) stands behind it.
//!
//! The stock kinds ([`stock`]) ship as default definitions, as the builtin
//! workflow templates do. A charter may add kinds, or tighten a stock kind by
//! extending it: an extension lists only the conditions it adds, so it can
//! never remove one. [`Catalog::resolve`] checks both against the vocabulary
//! and composes the extensions, and [`Catalog::freeze`] fixes the kinds an
//! admission uses into the convoy, so a charter edited mid-flight never
//! changes a running convoy's rules.

use std::{
    collections::{BTreeMap, BTreeSet},
    fmt,
};

pub use flotilla_address::Template;
use flotilla_address::{Address, Operator, Variable};
use flotilla_schema::Frozen;
use serde::{Deserialize, Serialize};

use crate::machines::convoy::{PromiseKind, PromiseKinds};

/// The observation behind a condition: what the observers record, and where.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Observation {
    /// The change request's state on its forge: open, draft, merged or closed.
    ChangeRequestState,
    /// The change request's checks at its head: pass, fail or pending.
    ChangeRequestChecks,
    /// Whether the forge reports the change request mergeable.
    ChangeRequestMergeable,
    /// The forge's review decision: approved, changes-requested or review-required.
    ChangeRequestReview,
    /// The commit at the change request's head.
    ChangeRequestHead,
    /// Whether the artifact store holds the artifact.
    ArtifactExists,
    /// The digest of the artifact the store holds.
    ArtifactDigest,
}

/// What a condition's literal may be.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Literal {
    /// One of these words.
    OneOf(&'static [&'static str]),
    /// The word `revision`, bound to the submission's revision: the commit a
    /// change request was submitted at, or an artifact's digest.
    Revision,
}

/// The literal that binds the submission's revision.
pub const REVISION: &str = "revision";

/// One shape of condition the vocabulary admits.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Term {
    pub subject: &'static str,
    pub path: &'static str,
    pub operators: &'static [Operator],
    pub literal: Literal,
    pub observation: Observation,
}

const EQUALITY: &[Operator] = &[Operator::Equal, Operator::NotEqual];

/// The condition vocabulary. "Checks settled" is `$cr .checks != pending`, and
/// "exists at digest" is `$artifact .digest == revision`.
pub const VOCABULARY: &[Term] = &[
    Term {
        subject: "cr",
        path: "state",
        operators: EQUALITY,
        literal: Literal::OneOf(&["open", "draft", "merged", "closed"]),
        observation: Observation::ChangeRequestState,
    },
    Term {
        subject: "cr",
        path: "checks",
        operators: EQUALITY,
        literal: Literal::OneOf(&["pass", "fail", "pending"]),
        observation: Observation::ChangeRequestChecks,
    },
    Term {
        subject: "cr",
        path: "mergeable",
        operators: EQUALITY,
        literal: Literal::OneOf(&["true", "false"]),
        observation: Observation::ChangeRequestMergeable,
    },
    Term {
        subject: "cr",
        path: "review",
        operators: EQUALITY,
        literal: Literal::OneOf(&["approved", "changes-requested", "review-required"]),
        observation: Observation::ChangeRequestReview,
    },
    Term { subject: "cr", path: "head-sha", operators: EQUALITY, literal: Literal::Revision, observation: Observation::ChangeRequestHead },
    Term {
        subject: "artifact",
        path: "exists",
        operators: &[Operator::Equal],
        literal: Literal::OneOf(&["true"]),
        observation: Observation::ArtifactExists,
    },
    Term { subject: "artifact", path: "digest", operators: EQUALITY, literal: Literal::Revision, observation: Observation::ArtifactDigest },
];

/// The vocabulary term a template uses, or why it uses none.
pub fn term(template: &Template) -> Result<&'static Term, TermError> {
    if !template.subject.arguments().is_empty() {
        return Err(TermError::Arguments);
    }
    let path = template.path.segments();
    let term = VOCABULARY
        .iter()
        .find(|term| term.subject == template.subject.name() && path.len() == 1 && path[0] == term.path)
        .ok_or(TermError::Unobserved)?;
    if !term.operators.contains(&template.operator) {
        return Err(TermError::Operator);
    }
    let admitted = match term.literal {
        Literal::OneOf(words) => words.contains(&template.literal.as_str()),
        Literal::Revision => template.literal == REVISION,
    };
    if !admitted {
        return Err(TermError::Literal);
    }
    Ok(term)
}

/// Why a template is outside the vocabulary.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TermError {
    /// The subject is the submission, which takes no arguments.
    Arguments,
    /// No observation stands behind this subject and path.
    Unobserved,
    Operator,
    Literal,
}

/// One of a kind's three rules.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Rule {
    /// The submitted reference survives: a submission is accepted only while it holds.
    Durable,
    /// The promise is worth acting on; a view, never written to the convoy.
    Ready,
    /// The observation that keeps the promise.
    Kept,
}

impl Rule {
    pub const ALL: [Rule; 3] = [Rule::Durable, Rule::Ready, Rule::Kept];
}

impl PromiseKind {
    pub fn rule(&self, rule: Rule) -> &[Template] {
        match rule {
            Rule::Durable => &self.durable,
            Rule::Ready => &self.ready,
            Rule::Kept => &self.kept,
        }
    }

    fn rule_mut(&mut self, rule: Rule) -> &mut Vec<Template> {
        match rule {
            Rule::Durable => &mut self.durable,
            Rule::Ready => &mut self.ready,
            Rule::Kept => &mut self.kept,
        }
    }

    /// Whether `rule` holds for a submission of `address` at `revision`: every
    /// condition holds, `None` while any is unobserved and none fails. An
    /// empty durable or ready rule holds; an empty kept rule never does, so
    /// only a verdict keeps that kind. A frozen condition a later generation's
    /// vocabulary no longer admits reads as unobserved: the rule stays `None`,
    /// never silently false, and the caller reports it ([`Catalog::retired`]).
    pub fn holds(&self, rule: Rule, address: &Address, revision: &str, observations: &impl Observations) -> Option<bool> {
        let conditions = self.rule(rule);
        if rule == Rule::Kept && conditions.is_empty() {
            return Some(false);
        }
        let mut known = true;
        for template in conditions {
            let Ok(term) = term(template) else {
                known = false;
                continue;
            };
            let literal = match term.literal {
                Literal::OneOf(_) => template.literal.as_str(),
                Literal::Revision => revision,
            };
            match observations.observe(term.observation, address) {
                None => known = false,
                Some(value) => {
                    if (value == literal) != (template.operator == Operator::Equal) {
                        return Some(false);
                    }
                }
            }
        }
        known.then_some(true)
    }
}

/// The observed facts a kind's rules read: each observation's current value
/// for a submitted address, as the vocabulary's words, or `None` while it is
/// unobserved or stale.
pub trait Observations {
    fn observe(&self, observation: Observation, address: &Address) -> Option<String>;
}

/// A kind as a stock or charter definition declares it. An extension names
/// the kind it extends and lists only the conditions it adds.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct KindDefinition {
    pub name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub extends: Option<String>,
    #[serde(default)]
    pub durable: Vec<Template>,
    #[serde(default)]
    pub ready: Vec<Template>,
    #[serde(default)]
    pub kept: Vec<Template>,
}

impl KindDefinition {
    fn rule(&self, rule: Rule) -> &[Template] {
        match rule {
            Rule::Durable => &self.durable,
            Rule::Ready => &self.ready,
            Rule::Kept => &self.kept,
        }
    }
}

fn template(text: &str) -> Template {
    text.parse().expect("stock conditions are canonical")
}

fn templates(texts: &[&str]) -> Vec<Template> {
    texts.iter().map(|text| template(text)).collect()
}

/// The stock kinds: `pr` is kept when its change request merges, and ready
/// when it is open, passing and mergeable; `decision-ledger` and `results`
/// are kept when the artifact store holds the artifact at the submitted
/// digest.
pub fn stock() -> Vec<KindDefinition> {
    let artifact = |name: &str| KindDefinition {
        name: name.into(),
        extends: None,
        durable: templates(&["$artifact .exists == true"]),
        ready: Vec::new(),
        kept: templates(&["$artifact .digest == revision"]),
    };
    vec![
        KindDefinition {
            name: "pr".into(),
            extends: None,
            durable: templates(&["$cr .state != closed"]),
            ready: templates(&["$cr .state == open", "$cr .checks == pass", "$cr .mergeable == true"]),
            kept: templates(&["$cr .state == merged"]),
        },
        artifact("decision-ledger"),
        artifact("results"),
    ]
}

/// Why a set of definitions does not resolve.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum KindError {
    /// A name that is not a lowercase word: `[a-z][a-z0-9-]*`.
    InvalidName(String),
    /// Two definitions of one name in one layer.
    Duplicate(String),
    /// A charter kind with a stock kind's name that does not extend it.
    StockRedeclared(String),
    UnknownBase {
        kind: String,
        extends: String,
    },
    Cycle(String),
    /// A condition outside the vocabulary.
    Unobserved {
        kind: String,
        rule: Rule,
        template: String,
        error: TermError,
    },
    /// Conditions over more than one subject variable: `extension` is the
    /// definition whose conditions brought in a second subject.
    MixedSubjects {
        kind: String,
        extension: String,
        subjects: Vec<String>,
    },
    /// An admission used a kind the catalog does not hold.
    UnknownKind(String),
    Encode(String),
}

impl fmt::Display for TermError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            TermError::Arguments => "the submission variable takes no arguments",
            TermError::Unobserved => "no observation stands behind this variable and path",
            TermError::Operator => "the vocabulary does not admit this operator here",
            TermError::Literal => "the vocabulary does not admit this literal here",
        })
    }
}

impl fmt::Display for Rule {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Rule::Durable => "durable",
            Rule::Ready => "ready",
            Rule::Kept => "kept",
        })
    }
}

impl fmt::Display for KindError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            KindError::InvalidName(name) => write!(f, "promise kind `{name}`: a kind name is a lowercase word ([a-z][a-z0-9-]*)"),
            KindError::Duplicate(name) => write!(f, "promise kind `{name}` is declared twice"),
            KindError::StockRedeclared(name) => {
                write!(f, "promise kind `{name}` is a stock kind: tighten it with `extends: {name}`, which only adds conditions")
            }
            KindError::UnknownBase { kind, extends } => write!(f, "promise kind `{kind}` extends `{extends}`, which is not a known kind"),
            KindError::Cycle(name) => write!(f, "promise kind `{name}` extends itself through its bases"),
            KindError::Unobserved { kind, rule, template, error } => {
                write!(f, "promise kind `{kind}`, {rule} rule: `{template}`: {error}")
            }
            KindError::MixedSubjects { kind, extension, subjects } => write!(
                f,
                "promise kind `{kind}`: `{extension}` adds conditions over a second submission variable ({})",
                subjects.iter().map(|subject| format!("${subject}")).collect::<Vec<_>>().join(", ")
            ),
            KindError::UnknownKind(name) => write!(f, "promise kind `{name}` is not in the charter's catalog"),
            KindError::Encode(error) => write!(f, "the frozen promise kinds do not encode: {error}"),
        }
    }
}

impl std::error::Error for KindError {}

/// The kinds in force for one charter: the stock kinds with its own composed
/// on top, every condition checked against the vocabulary.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Catalog {
    kinds: BTreeMap<String, PromiseKind>,
}

impl Catalog {
    /// Composes `stock` and a charter's `declared` kinds. A charter kind with
    /// a stock kind's name must extend that stock kind, tightening it in
    /// place; any other extension names a stock kind or another charter kind.
    pub fn resolve(stock: &[KindDefinition], declared: &[KindDefinition]) -> Result<Self, KindError> {
        let stock = layer(stock)?;
        let declared = layer(declared)?;
        let mut kinds = BTreeMap::new();
        for (name, definition) in &stock {
            let kind = compose(name, definition, &stock, &BTreeMap::new(), &mut BTreeSet::new()).map_err(|error| top(error, name))?;
            kinds.insert(name.clone(), kind);
        }
        for (name, definition) in &declared {
            if stock.contains_key(name) && definition.extends.as_deref() != Some(name.as_str()) {
                return Err(KindError::StockRedeclared(name.clone()));
            }
            let kind = compose(name, definition, &stock, &declared, &mut BTreeSet::new()).map_err(|error| top(error, name))?;
            kinds.insert(name.clone(), kind);
        }
        Ok(Self { kinds })
    }

    pub fn get(&self, name: &str) -> Option<&PromiseKind> {
        self.kinds.get(name)
    }

    pub fn names(&self) -> impl Iterator<Item = &str> {
        self.kinds.keys().map(String::as_str)
    }

    /// The conditions of frozen `kinds` that this generation's vocabulary no
    /// longer admits, by kind and rule: each reads as unobserved in
    /// [`PromiseKind::holds`], so the caller reports them rather than wait
    /// silently.
    pub fn retired(kinds: &PromiseKinds) -> Vec<(String, Rule, Template)> {
        kinds
            .kinds
            .iter()
            .flat_map(|(name, kind)| {
                Rule::ALL.into_iter().flat_map(move |rule| {
                    kind.rule(rule)
                        .iter()
                        .filter(|template| term(template).is_err())
                        .map(move |template| (name.clone(), rule, template.clone()))
                })
            })
            .collect()
    }

    /// The kinds `used` names, frozen for a convoy's admission.
    pub fn freeze<'a>(&self, used: impl IntoIterator<Item = &'a str>) -> Result<Frozen<PromiseKinds>, KindError> {
        let mut kinds = BTreeMap::new();
        for name in used {
            let kind = self.kinds.get(name).ok_or_else(|| KindError::UnknownKind(name.to_owned()))?;
            kinds.insert(name.to_owned(), kind.clone());
        }
        Frozen::new(PromiseKinds { kinds }).map_err(|error| KindError::Encode(error.to_string()))
    }
}

fn layer(definitions: &[KindDefinition]) -> Result<BTreeMap<String, KindDefinition>, KindError> {
    let mut layer = BTreeMap::new();
    for definition in definitions {
        if Variable::new(definition.name.as_str(), Vec::new()).is_err() || definition.name.contains('_') {
            return Err(KindError::InvalidName(definition.name.clone()));
        }
        for rule in Rule::ALL {
            for template in definition.rule(rule) {
                term(template).map_err(|error| KindError::Unobserved {
                    kind: definition.name.clone(),
                    rule,
                    template: template.to_string(),
                    error,
                })?;
            }
        }
        if layer.insert(definition.name.clone(), definition.clone()).is_some() {
            return Err(KindError::Duplicate(definition.name.clone()));
        }
    }
    Ok(layer)
}

/// A kind's name on a mixed-subject refusal raised while composing one of its bases.
fn top(error: KindError, name: &str) -> KindError {
    match error {
        KindError::MixedSubjects { extension, subjects, .. } => KindError::MixedSubjects { kind: name.to_owned(), extension, subjects },
        error => error,
    }
}

/// A definition's rules: its base's, then its own. A charter kind's base is
/// the charter kind of that name, unless it extends itself, which is the
/// stock kind's. A definition names one base, so its bases form a chain, never
/// a diamond: `visiting` holds that chain for one top-level kind (each
/// resolution starts a fresh set), and meeting a name again is a cycle.
fn compose(
    name: &str,
    definition: &KindDefinition,
    stock: &BTreeMap<String, KindDefinition>,
    declared: &BTreeMap<String, KindDefinition>,
    visiting: &mut BTreeSet<String>,
) -> Result<PromiseKind, KindError> {
    let none = BTreeMap::new();
    let mut kind = match definition.extends.as_deref() {
        None => PromiseKind::default(),
        Some(base) => {
            if !visiting.insert(name.to_owned()) {
                return Err(KindError::Cycle(name.to_owned()));
            }
            let (definition, below) = match declared.get(base) {
                Some(found) if base != name => (found, declared),
                _ => match stock.get(base) {
                    Some(found) if base != name || declared.contains_key(name) => (found, &none),
                    _ => return Err(KindError::UnknownBase { kind: name.to_owned(), extends: base.to_owned() }),
                },
            };
            compose(base, definition, stock, below, visiting)?
        }
    };
    for rule in Rule::ALL {
        for template in definition.rule(rule) {
            if !kind.rule(rule).contains(template) {
                kind.rule_mut(rule).push(template.clone());
            }
        }
    }
    let subjects: BTreeSet<&str> = Rule::ALL.iter().flat_map(|rule| kind.rule(*rule)).map(|template| template.subject.name()).collect();
    if subjects.len() > 1 {
        return Err(KindError::MixedSubjects {
            kind: name.to_owned(),
            extension: name.to_owned(),
            subjects: subjects.into_iter().map(str::to_owned).collect(),
        });
    }
    Ok(kind)
}
