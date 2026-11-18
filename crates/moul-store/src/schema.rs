//! The app definition: collections, their fields and their rules.
//!
//! These types read and write the JSON the Go engine uses for a collection
//! (its `Moul` type), so a Go database converts without loss and the HTTP
//! API answers with the same shapes. Two things are added for sharing:
//!
//! - **Field ids.** Every field has an `id` that outlives a rename. A
//!   definition that arrives without ids gets them matched by name to the
//!   previous definition, or new ones (see [`Collection::match_field_ids`]).
//!   Columns keep the field's name, so the file stays readable by other
//!   tools; a rename becomes `ALTER TABLE … RENAME COLUMN`.
//! - **No secrets.** Webhook secrets and auth credentials live in the
//!   writer-only store, never in a collection's definition.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::fmt;

use serde::de::Deserializer;
use serde::{Deserialize, Serialize, Serializer};

/// A collection: one table of records, its fields and its access rules.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Collection {
    /// Stable id, 15 alphanumeric characters. Survives renames.
    #[serde(default, deserialize_with = "null_as_default")]
    pub id: String,
    /// Name, which is also the table name.
    #[serde(default, deserialize_with = "null_as_default")]
    pub name: String,
    /// What kind of collection this is.
    #[serde(rename = "type", default)]
    pub kind: CollectionKind,
    /// The fields the builder defined, in order. System columns are not listed.
    #[serde(default, deserialize_with = "null_as_default")]
    pub fields: Vec<Field>,
    /// Who may list, view, create, update, delete and subscribe.
    #[serde(default, deserialize_with = "null_as_default")]
    pub rules: Rules,
    /// Mail templates of an auth collection.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub email_templates: Option<EmailTemplates>,
    /// Webhooks kept from a Go database, without their secrets. Moul does
    /// not call them yet.
    #[serde(
        default,
        skip_serializing_if = "Vec::is_empty",
        deserialize_with = "null_as_default"
    )]
    pub webhooks: Vec<serde_json::Value>,
    /// RFC 3339, UTC, whole seconds.
    #[serde(default, deserialize_with = "null_as_default")]
    pub created_at: String,
    /// RFC 3339, UTC, whole seconds.
    #[serde(default, deserialize_with = "null_as_default")]
    pub updated_at: String,
}

/// What a collection holds.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub enum CollectionKind {
    /// Plain records.
    #[default]
    Base,
    /// People who sign in: adds `username`, `email` and credentials.
    Auth,
    /// Background jobs (Go engine). Kept when a Go database converts; not
    /// served yet.
    Worker,
    /// Analytics events (Go engine). Kept when a Go database converts; not
    /// served yet.
    Analytic,
}

impl CollectionKind {
    /// The name used in JSON and in `_moul.type`.
    pub const fn as_str(self) -> &'static str {
        match self {
            CollectionKind::Base => "base",
            CollectionKind::Auth => "auth",
            CollectionKind::Worker => "worker",
            CollectionKind::Analytic => "analytic",
        }
    }

    /// Parses a stored or requested type. Anything unknown is a base
    /// collection, as in the Go engine.
    pub fn parse(s: &str) -> CollectionKind {
        match s {
            "auth" => CollectionKind::Auth,
            "worker" => CollectionKind::Worker,
            "analytic" => CollectionKind::Analytic,
            _ => CollectionKind::Base,
        }
    }

    /// Columns the engine manages for this kind, lowercase. Every kind has
    /// `id`, `createdat` and `updatedat`.
    pub fn system_columns(self) -> &'static [&'static str] {
        match self {
            CollectionKind::Base => &["id", "createdat", "updatedat"],
            CollectionKind::Auth => &["id", "createdat", "updatedat", "username", "email"],
            CollectionKind::Worker => &[
                "id",
                "createdat",
                "updatedat",
                "state",
                "queue",
                "worker",
                "args",
                "meta",
                "tags",
                "errors",
                "attempt",
                "max_attempts",
                "priority",
                "inserted_at",
                "scheduled_at",
                "attempted_at",
                "attempted_by",
                "cancelled_at",
                "completed_at",
                "discarded_at",
            ],
            CollectionKind::Analytic => &[
                "id",
                "createdat",
                "updatedat",
                "visit_token",
                "visitor_token",
                "user_id",
                "name",
                "properties",
                "time",
            ],
        }
    }

    /// Names a builder may not give a field of this kind, lowercase. Auth
    /// collections also reserve the credential names the Go engine used.
    pub fn reserved_field_names(self) -> &'static [&'static str] {
        match self {
            CollectionKind::Auth => &[
                "id",
                "createdat",
                "updatedat",
                "username",
                "email",
                "passwordhash",
                "password",
                "otpcode",
                "otpexpiresat",
                "passkeys",
                "resettoken",
                "resettokenexpiresat",
                "oauthproviders",
            ],
            _ => &["id", "createdat", "updatedat"],
        }
    }
}

impl fmt::Display for CollectionKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl Serialize for CollectionKind {
    fn serialize<S: Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        s.serialize_str(self.as_str())
    }
}

impl<'de> Deserialize<'de> for CollectionKind {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        let s = Option::<String>::deserialize(d)?.unwrap_or_default();
        Ok(CollectionKind::parse(&s))
    }
}

/// One field of a collection.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Field {
    /// Stable id, `fld_` and 8 alphanumeric characters. Survives renames.
    #[serde(
        default,
        skip_serializing_if = "String::is_empty",
        deserialize_with = "null_as_default"
    )]
    pub id: String,
    /// Name, which is also the column name. camelCase.
    #[serde(default, deserialize_with = "null_as_default")]
    pub name: String,
    /// The kind of value. A field given without one has an empty unknown
    /// type, which [`Collection::validate`] refuses, as Go does.
    #[serde(rename = "type", default = "FieldType::missing")]
    pub kind: FieldType,
    /// A value must be given and not empty.
    #[serde(
        default,
        skip_serializing_if = "is_false",
        deserialize_with = "null_as_default"
    )]
    pub required: bool,
    /// Lower bound: a number's value, or a text's length.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub min: Option<f64>,
    /// Upper bound: a number's value, or a text's length.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max: Option<f64>,
    /// The choices of a `select` field. Kept as given on other types, as
    /// in Go.
    #[serde(
        default,
        skip_serializing_if = "Vec::is_empty",
        deserialize_with = "null_as_default"
    )]
    pub options: Vec<String>,
    /// The target of a `relation` field. Kept as given on other types, as
    /// in Go.
    #[serde(
        default,
        rename = "relationConfig",
        skip_serializing_if = "Option::is_none"
    )]
    pub relation: Option<Relation>,
    /// A `cloak` field that can be matched by equality.
    #[serde(
        default,
        skip_serializing_if = "is_false",
        deserialize_with = "null_as_default"
    )]
    pub searchable: bool,
    /// No two records may hold the same value.
    #[serde(
        default,
        skip_serializing_if = "is_false",
        deserialize_with = "null_as_default"
    )]
    pub unique: bool,
}

impl Field {
    /// A field with only a name and a type.
    pub fn new(name: impl Into<String>, kind: FieldType) -> Field {
        Field {
            id: String::new(),
            name: name.into(),
            kind,
            required: false,
            min: None,
            max: None,
            options: Vec::new(),
            relation: None,
            searchable: false,
            unique: false,
        }
    }
}

/// The kind of value a field holds.
#[derive(Clone, Debug, Default, PartialEq, Eq, Hash)]
pub enum FieldType {
    /// Text.
    #[default]
    Text,
    /// A number, stored as SQLite `NUMERIC`.
    Number,
    /// True or false, stored as 0 or 1.
    Bool,
    /// A date, `YYYY-MM-DD`.
    Date,
    /// A date and time, RFC 3339.
    Datetime,
    /// Any JSON value, stored as text.
    Json,
    /// An absolute `http` or `https` URL.
    Url,
    /// An email address.
    Email,
    /// One uploaded file, or a list of them.
    File,
    /// The id of a record in another collection, or a list of them.
    Relation,
    /// One of the field's options.
    Select,
    /// An encrypted value (Go engine). Kept when a Go database converts;
    /// new ones cannot be made yet.
    Cloak,
    /// A type this engine does not know. Refused when a definition is checked.
    Unknown(String),
}

impl FieldType {
    /// The type of a field given without one: unknown and empty.
    fn missing() -> FieldType {
        FieldType::Unknown(String::new())
    }

    /// Every type a new field may have.
    pub const SUPPORTED: [FieldType; 11] = [
        FieldType::Text,
        FieldType::Number,
        FieldType::Bool,
        FieldType::Date,
        FieldType::Datetime,
        FieldType::Json,
        FieldType::Url,
        FieldType::Email,
        FieldType::File,
        FieldType::Relation,
        FieldType::Select,
    ];

    /// The name used in JSON.
    pub fn as_str(&self) -> &str {
        match self {
            FieldType::Text => "text",
            FieldType::Number => "number",
            FieldType::Bool => "bool",
            FieldType::Date => "date",
            FieldType::Datetime => "datetime",
            FieldType::Json => "json",
            FieldType::Url => "url",
            FieldType::Email => "email",
            FieldType::File => "file",
            FieldType::Relation => "relation",
            FieldType::Select => "select",
            FieldType::Cloak => "cloak",
            FieldType::Unknown(s) => s,
        }
    }

    /// Parses a type name.
    pub fn parse(s: &str) -> FieldType {
        match s {
            "text" => FieldType::Text,
            "number" => FieldType::Number,
            "bool" => FieldType::Bool,
            "date" => FieldType::Date,
            "datetime" => FieldType::Datetime,
            "json" => FieldType::Json,
            "url" => FieldType::Url,
            "email" => FieldType::Email,
            "file" => FieldType::File,
            "relation" => FieldType::Relation,
            "select" => FieldType::Select,
            "cloak" => FieldType::Cloak,
            other => FieldType::Unknown(other.to_owned()),
        }
    }

    /// The SQLite column type, as the Go engine declares it.
    pub fn column_type(&self) -> &'static str {
        match self {
            FieldType::Cloak => "BLOB",
            FieldType::Number => "NUMERIC",
            FieldType::Bool => "INTEGER",
            _ => "TEXT",
        }
    }

    /// Whether a unique index may be put on a field of this type.
    pub fn can_be_unique(&self) -> bool {
        matches!(
            self,
            FieldType::Text
                | FieldType::Number
                | FieldType::Date
                | FieldType::Datetime
                | FieldType::Url
                | FieldType::Email
        )
    }
}

impl fmt::Display for FieldType {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl Serialize for FieldType {
    fn serialize<S: Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        s.serialize_str(self.as_str())
    }
}

impl<'de> Deserialize<'de> for FieldType {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        let s = Option::<String>::deserialize(d)?.unwrap_or_default();
        Ok(FieldType::parse(&s))
    }
}

/// Where a relation field points.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Relation {
    /// The target collection's name. Renaming the target updates it.
    #[serde(default, deserialize_with = "null_as_default")]
    pub target_moul: String,
    /// `1:1`, `1:N` or `M:N`. `M:N` holds a list of ids.
    #[serde(default, deserialize_with = "null_as_default")]
    pub cardinality: String,
    /// `CASCADE`, `SET_NULL` (the default) or `RESTRICT`.
    #[serde(
        default,
        skip_serializing_if = "String::is_empty",
        deserialize_with = "null_as_default"
    )]
    pub on_delete: String,
}

impl Relation {
    /// Whether the field holds a list of ids.
    pub fn is_many(&self) -> bool {
        self.cardinality == "M:N"
    }
}

/// The access rules of a collection, in the rule language. An empty rule
/// lets anyone through, as in the Go engine.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Rules {
    /// Who may list records, and which ones they see.
    #[serde(default, deserialize_with = "null_as_default")]
    pub list_rule: String,
    /// Who may view one record.
    #[serde(default, deserialize_with = "null_as_default")]
    pub view_rule: String,
    /// Who may create a record.
    #[serde(default, deserialize_with = "null_as_default")]
    pub create_rule: String,
    /// Who may update a record.
    #[serde(default, deserialize_with = "null_as_default")]
    pub update_rule: String,
    /// Who may delete a record.
    #[serde(default, deserialize_with = "null_as_default")]
    pub delete_rule: String,
    /// Who receives live changes. Empty means the view rule decides.
    #[serde(
        default,
        skip_serializing_if = "String::is_empty",
        deserialize_with = "null_as_default"
    )]
    pub subscribe_rule: String,
}

/// The rule an auth collection gets for list, view, update and delete when
/// none is given: people see and change only their own record.
pub const AUTH_SELF_RULE: &str = "id = @request.auth.id";

/// One mail template. `{{.Link}}` and `{{.OTP}}` are filled in.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct EmailTemplate {
    /// Subject line.
    #[serde(default, deserialize_with = "null_as_default")]
    pub subject: String,
    /// Plain-text body.
    #[serde(default, deserialize_with = "null_as_default")]
    pub body: String,
}

/// The mail templates of an auth collection.
///
/// [`Default`] gives the built-in templates, which a collection with no
/// templates at all gets. A template left out of JSON reads as empty, as
/// in Go.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct EmailTemplates {
    /// Confirming an address.
    #[serde(default, deserialize_with = "null_as_default")]
    pub verification: EmailTemplate,
    /// Resetting a password.
    #[serde(default, deserialize_with = "null_as_default")]
    pub password_reset: EmailTemplate,
    /// Confirming a new address.
    #[serde(default, deserialize_with = "null_as_default")]
    pub confirm_email_change: EmailTemplate,
    /// A one-time sign-in code.
    #[serde(default, deserialize_with = "null_as_default")]
    pub otp: EmailTemplate,
    /// A new sign-in happened.
    #[serde(default, deserialize_with = "null_as_default")]
    pub login_alert: EmailTemplate,
}

impl Default for EmailTemplates {
    fn default() -> Self {
        EmailTemplates {
            verification: default_verification(),
            password_reset: default_password_reset(),
            confirm_email_change: default_confirm_email_change(),
            otp: default_otp(),
            login_alert: default_login_alert(),
        }
    }
}

fn template(subject: &str, body: &str) -> EmailTemplate {
    EmailTemplate {
        subject: subject.into(),
        body: body.into(),
    }
}

fn default_verification() -> EmailTemplate {
    template(
        "Confirm your email address",
        "Hello,\n\nPlease verify your email by clicking on the link below:\n\n{{.Link}}\n\nRegards,\nSupport Team",
    )
}

fn default_password_reset() -> EmailTemplate {
    template(
        "Reset your password",
        "Hello,\n\nYou requested a password reset. Please use the link below to set a new password:\n\n{{.Link}}\n\nIf you did not request this, you can ignore this email.\n\nRegards,\nSupport Team",
    )
}

fn default_confirm_email_change() -> EmailTemplate {
    template(
        "Confirm email change",
        "Hello,\n\nPlease confirm your email address change by clicking on the link below:\n\n{{.Link}}\n\nRegards,\nSupport Team",
    )
}

fn default_otp() -> EmailTemplate {
    template(
        "Your OTP Code",
        "Hello,\n\nYour one-time password (OTP) code is:\n\n{{.OTP}}\n\nThis code will expire in 10 minutes.\n\nRegards,\nSupport Team",
    )
}

fn default_login_alert() -> EmailTemplate {
    template(
        "New Login Alert",
        "Hello,\n\nA new login was detected on your account.\n\nIf this was not you, please secure your account immediately.\n\nRegards,\nSupport Team",
    )
}

/// A definition that cannot be accepted. The message is shown to the
/// builder as is, and matches the Go engine's where it has one.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Invalid(pub String);

impl fmt::Display for Invalid {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for Invalid {}

fn invalid<T>(msg: impl Into<String>) -> Result<T, Invalid> {
    Err(Invalid(msg.into()))
}

/// The message for a name that is not a safe identifier.
pub const INVALID_NAME: &str = "Invalid moul name: must start with a letter and contain only letters, digits, or underscores (max 63 chars)";

/// Whether `name` is a safe collection (table) name: a letter, then up to
/// 62 letters, digits or underscores.
pub fn is_valid_collection_name(name: &str) -> bool {
    let b = name.as_bytes();
    !b.is_empty()
        && b.len() <= 63
        && b[0].is_ascii_alphabetic()
        && b.iter().all(|c| c.is_ascii_alphanumeric() || *c == b'_')
}

/// Whether `name` is a valid field name: camelCase, starting lowercase.
pub fn is_valid_field_name(name: &str) -> bool {
    let b = name.as_bytes();
    !b.is_empty() && b[0].is_ascii_lowercase() && b.iter().all(|c| c.is_ascii_alphanumeric())
}

/// Whether `id` has the shape of a collection id: 15 alphanumeric characters.
/// Ids made by the Go engine and by Moul have it; a sender may choose its own.
pub fn is_valid_collection_id(id: &str) -> bool {
    (1..=64).contains(&id.len())
        && id
            .bytes()
            .all(|c| c.is_ascii_alphanumeric() || c == b'_' || c == b'-')
}

/// Whether `id` can be a field id: `fld_` and 1 to 32 alphanumeric characters.
pub fn is_valid_field_id(id: &str) -> bool {
    id.strip_prefix("fld_").is_some_and(|rest| {
        (1..=32).contains(&rest.len()) && rest.bytes().all(|c| c.is_ascii_alphanumeric())
    })
}

/// Quotes an identifier for SQL with backticks, as the Go engine does.
pub fn quote(name: &str) -> String {
    format!("`{}`", name.replace('`', "``"))
}

impl Collection {
    /// An empty collection of `kind` named `name`.
    pub fn new(name: impl Into<String>, kind: CollectionKind) -> Collection {
        Collection {
            id: String::new(),
            name: name.into(),
            kind,
            fields: Vec::new(),
            rules: Rules::default(),
            email_templates: None,
            webhooks: Vec::new(),
            created_at: String::new(),
            updated_at: String::new(),
        }
    }

    /// The field named `name`, compared without case as SQLite does.
    pub fn field(&self, name: &str) -> Option<&Field> {
        self.fields
            .iter()
            .find(|f| f.name.eq_ignore_ascii_case(name))
    }

    /// The field with id `id`.
    pub fn field_by_id(&self, id: &str) -> Option<&Field> {
        self.fields.iter().find(|f| f.id == id)
    }

    /// Whether `name` is a column the engine manages for this kind.
    pub fn is_system_column(&self, name: &str) -> bool {
        let lower = name.to_ascii_lowercase();
        self.kind.system_columns().contains(&lower.as_str())
    }

    /// The fields that become columns: everything except names that clash
    /// with a system column (which the Go engine also skipped).
    pub fn column_fields(&self) -> impl Iterator<Item = &Field> {
        self.fields
            .iter()
            .filter(|f| !self.is_system_column(&f.name))
    }

    /// Cleans and checks the definition the way the Go engine's collection
    /// handlers do: trims names, defaults auth rules, normalises the
    /// settings of relation and select fields, and refuses what it cannot
    /// store. Options and relation settings on fields of other types are
    /// kept as given, as in Go.
    ///
    /// `previous` is the stored definition when this is an update. A `cloak`
    /// field is accepted only if `previous` already had it, because this
    /// engine cannot encrypt new ones yet.
    pub fn validate(&mut self, previous: Option<&Collection>) -> Result<(), Invalid> {
        self.name = self.name.trim().to_owned();
        if self.name.is_empty() {
            return invalid("Moul name is required");
        }
        if self.name.starts_with('_') {
            return invalid("Moul name cannot start with underscore");
        }
        if !is_valid_collection_name(&self.name) {
            return invalid(INVALID_NAME);
        }
        if let Some(prev) = previous
            && prev.kind != self.kind
        {
            return invalid(format!(
                "cannot change collection type from {:?} to {:?}",
                prev.kind.as_str(),
                self.kind.as_str()
            ));
        }
        if self.kind == CollectionKind::Auth {
            for rule in [
                &mut self.rules.list_rule,
                &mut self.rules.view_rule,
                &mut self.rules.update_rule,
                &mut self.rules.delete_rule,
            ] {
                if rule.is_empty() {
                    *rule = AUTH_SELF_RULE.to_owned();
                }
            }
            if self.email_templates.is_none() {
                self.email_templates = Some(EmailTemplates::default());
            }
        } else {
            self.email_templates = None;
        }

        let reserved = self.kind.reserved_field_names();
        let mut seen = HashSet::new();
        let mut ids = HashSet::new();
        for f in &mut self.fields {
            f.name = f.name.trim().to_owned();
            if f.name.is_empty() {
                return invalid("field name cannot be empty");
            }
            if !is_valid_field_name(&f.name) {
                return invalid(format!(
                    "field name {:?} must be camelCase (e.g. \"authorId\")",
                    f.name
                ));
            }
            let lower = f.name.to_ascii_lowercase();
            if !seen.insert(lower.clone()) {
                return invalid(format!("duplicate field name {:?}", f.name));
            }
            if ["id", "createdat", "updatedat"].contains(&lower.as_str()) {
                return invalid(format!(
                    "field name {:?} is a reserved system column",
                    f.name
                ));
            }
            if reserved.contains(&lower.as_str()) {
                return invalid(format!(
                    "field name {:?} is a reserved column for auth collections",
                    f.name
                ));
            }
            if !f.id.is_empty() {
                if !is_valid_field_id(&f.id) {
                    return invalid(format!(
                        "field id {:?} of {:?} must be \"fld_\" and up to 32 letters or digits",
                        f.id, f.name
                    ));
                }
                if !ids.insert(f.id.clone()) {
                    return invalid(format!("duplicate field id {:?}", f.id));
                }
            }
            match &f.kind {
                FieldType::Text
                | FieldType::Number
                | FieldType::Bool
                | FieldType::Date
                | FieldType::Datetime
                | FieldType::Json
                | FieldType::Url
                | FieldType::File
                | FieldType::Email => {}
                FieldType::Cloak => {
                    let kept = previous.is_some_and(|p| {
                        p.fields.iter().any(|pf| {
                            pf.kind == FieldType::Cloak
                                && (pf.id == f.id
                                    || (f.id.is_empty() && pf.name.eq_ignore_ascii_case(&f.name)))
                        })
                    });
                    if !kept {
                        return invalid(format!(
                            "cannot use cloak field {:?}: encrypted fields are not supported yet",
                            f.name
                        ));
                    }
                }
                FieldType::Select => {
                    let cleaned: Vec<String> = f
                        .options
                        .iter()
                        .map(|o| o.trim().to_owned())
                        .filter(|o| !o.is_empty())
                        .collect();
                    if cleaned.is_empty() {
                        return invalid(format!(
                            "select field {:?} requires at least one option",
                            f.name
                        ));
                    }
                    f.options = cleaned;
                }
                FieldType::Relation => {
                    let Some(rel) = f.relation.as_mut() else {
                        return invalid(format!(
                            "relation field {:?} requires relationConfig",
                            f.name
                        ));
                    };
                    rel.target_moul = rel.target_moul.trim().to_owned();
                    if rel.target_moul.is_empty() {
                        return invalid(format!("relation field {:?} requires targetMoul", f.name));
                    }
                    rel.cardinality = rel.cardinality.trim().to_owned();
                    if !["1:1", "1:N", "M:N"].contains(&rel.cardinality.as_str()) {
                        return invalid(format!(
                            "invalid cardinality {:?} for field {:?} (allowed: 1:1, 1:N, M:N)",
                            rel.cardinality, f.name
                        ));
                    }
                    let on_delete = rel.on_delete.trim().to_ascii_uppercase();
                    let on_delete = if on_delete.is_empty() {
                        "SET_NULL".to_owned()
                    } else {
                        on_delete
                    };
                    if !["CASCADE", "SET_NULL", "RESTRICT"].contains(&on_delete.as_str()) {
                        return invalid(format!(
                            "invalid onDelete {:?} for field {:?} (allowed: CASCADE, SET_NULL, RESTRICT)",
                            rel.on_delete, f.name
                        ));
                    }
                    rel.on_delete = on_delete;
                }
                FieldType::Unknown(t) => {
                    return invalid(format!("invalid type {:?} for field {:?}", t, f.name));
                }
            }
            if f.unique {
                if !f.kind.can_be_unique() {
                    return invalid(format!(
                        "field {:?} of type {:?} cannot have a unique constraint",
                        f.name,
                        f.kind.as_str()
                    ));
                }
                f.required = true;
            }
        }
        Ok(())
    }

    /// Gives every field an id. A field that already has one keeps it; one
    /// without takes the id of the previous definition's field with the
    /// same name, or a new one from `new_id`. Returns the ids that were new.
    pub fn match_field_ids(
        &mut self,
        previous: Option<&Collection>,
        mut new_id: impl FnMut() -> String,
    ) -> Vec<String> {
        let mut taken: HashSet<String> = self
            .fields
            .iter()
            .filter(|f| !f.id.is_empty())
            .map(|f| f.id.clone())
            .collect();
        let by_name: HashMap<String, &str> = previous
            .map(|p| {
                p.fields
                    .iter()
                    .filter(|f| !f.id.is_empty())
                    .map(|f| (f.name.to_ascii_lowercase(), f.id.as_str()))
                    .collect()
            })
            .unwrap_or_default();
        let mut fresh = Vec::new();
        for f in &mut self.fields {
            if !f.id.is_empty() {
                continue;
            }
            if let Some(id) = by_name.get(&f.name.to_ascii_lowercase())
                && !taken.contains(*id)
            {
                f.id = (*id).to_owned();
                taken.insert(f.id.clone());
                continue;
            }
            loop {
                let id = new_id();
                if taken.insert(id.clone()) {
                    f.id = id.clone();
                    fresh.push(id);
                    break;
                }
            }
        }
        fresh
    }

    /// The JSON the HTTP API returns for this collection.
    pub fn to_json(&self) -> serde_json::Value {
        serde_json::to_value(self).expect("a collection always serialises")
    }
}

/// What changed between two definitions of a collection, matched by field id.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct FieldChanges {
    /// Fields only in the new definition.
    pub added: Vec<String>,
    /// Fields only in the old definition.
    pub removed: Vec<String>,
    /// Fields whose name changed: id, old name, new name.
    pub renamed: Vec<(String, String, String)>,
    /// Fields whose type, options, bounds or flags changed.
    pub changed: Vec<String>,
}

impl FieldChanges {
    /// Compares `old` and `new` field by field. Both must have ids.
    pub fn between(old: &Collection, new: &Collection) -> FieldChanges {
        let old_by_id: BTreeMap<&str, &Field> =
            old.fields.iter().map(|f| (f.id.as_str(), f)).collect();
        let new_ids: HashSet<&str> = new.fields.iter().map(|f| f.id.as_str()).collect();
        let mut out = FieldChanges::default();
        for f in &new.fields {
            match old_by_id.get(f.id.as_str()) {
                None => out.added.push(f.id.clone()),
                Some(o) => {
                    if o.name != f.name {
                        out.renamed
                            .push((f.id.clone(), o.name.clone(), f.name.clone()));
                    }
                    let mut a = (*o).clone();
                    a.name = f.name.clone();
                    if &a != f {
                        out.changed.push(f.id.clone());
                    }
                }
            }
        }
        for f in &old.fields {
            if !new_ids.contains(f.id.as_str()) {
                out.removed.push(f.id.clone());
            }
        }
        out
    }

    /// Whether nothing changed.
    pub fn is_empty(&self) -> bool {
        self.added.is_empty()
            && self.removed.is_empty()
            && self.renamed.is_empty()
            && self.changed.is_empty()
    }
}

fn is_false(b: &bool) -> bool {
    !*b
}

fn null_as_default<'de, D, T>(d: D) -> Result<T, D::Error>
where
    D: Deserializer<'de>,
    T: Default + Deserialize<'de>,
{
    Ok(Option::<T>::deserialize(d)?.unwrap_or_default())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_a_go_collection() {
        let json = r#"{"createdAt":"2026-11-18T00:32:44Z","fields":[{"name":"title","type":"text"},
            {"name":"team","relationConfig":{"cardinality":"1:N","targetMoul":"teams"},"type":"relation"}],
            "id":"o1I5HQ44w6iU5EP","name":"posts","rules":{"createRule":"","deleteRule":"",
            "listRule":"team.owner = @request.auth.id","updateRule":"","viewRule":""},
            "type":"base","updatedAt":"2026-11-18T00:32:44Z"}"#;
        let c: Collection = serde_json::from_str(json).unwrap();
        assert_eq!(c.kind, CollectionKind::Base);
        assert_eq!(c.fields[1].relation.as_ref().unwrap().target_moul, "teams");
        assert_eq!(c.rules.list_rule, "team.owner = @request.auth.id");
        let back = c.to_json();
        assert_eq!(
            back["fields"][0],
            serde_json::json!({"name": "title", "type": "text"})
        );
        assert!(back.get("emailTemplates").is_none());
    }

    #[test]
    fn null_fields_read_as_empty() {
        let c: Collection =
            serde_json::from_str(r#"{"name":"jobs","type":"worker","fields":null}"#).unwrap();
        assert!(c.fields.is_empty());
        assert_eq!(c.kind, CollectionKind::Worker);
    }

    #[test]
    fn auth_rules_default_to_self() {
        let mut c = Collection::new("users", CollectionKind::Auth);
        c.validate(None).unwrap();
        assert_eq!(c.rules.view_rule, AUTH_SELF_RULE);
        assert_eq!(c.rules.create_rule, "");
        assert!(c.email_templates.is_some());
    }

    #[test]
    fn rejects_bad_definitions() {
        let mut c = Collection::new("_x", CollectionKind::Base);
        assert_eq!(
            c.validate(None).unwrap_err().0,
            "Moul name cannot start with underscore"
        );
        let mut c = Collection::new("posts", CollectionKind::Base);
        c.fields.push(Field::new("Title", FieldType::Text));
        assert!(c.validate(None).unwrap_err().0.contains("camelCase"));
        let mut c = Collection::new("users", CollectionKind::Auth);
        c.fields.push(Field::new("password", FieldType::Text));
        assert!(
            c.validate(None)
                .unwrap_err()
                .0
                .contains("reserved column for auth")
        );
        let mut c = Collection::new("posts", CollectionKind::Base);
        c.fields.push(Field::new("secret", FieldType::Cloak));
        assert!(
            c.validate(None)
                .unwrap_err()
                .0
                .contains("not supported yet")
        );
        let mut c = Collection::new("posts", CollectionKind::Base);
        c.fields.push(Field::new("x", FieldType::parse("money")));
        assert_eq!(
            c.validate(None).unwrap_err().0,
            "invalid type \"money\" for field \"x\""
        );
    }

    #[test]
    fn unique_implies_required_and_relation_defaults() {
        let mut c = Collection::new("posts", CollectionKind::Base);
        let mut slug = Field::new("slug", FieldType::Text);
        slug.unique = true;
        let mut team = Field::new("team", FieldType::Relation);
        team.relation = Some(Relation {
            target_moul: " teams ".into(),
            cardinality: "1:N".into(),
            on_delete: String::new(),
        });
        c.fields = vec![slug, team];
        c.validate(None).unwrap();
        assert!(c.fields[0].required);
        let rel = c.fields[1].relation.as_ref().unwrap();
        assert_eq!(
            (rel.target_moul.as_str(), rel.on_delete.as_str()),
            ("teams", "SET_NULL")
        );
    }

    #[test]
    fn templates_left_out_are_empty_as_in_go() {
        let t: EmailTemplates = serde_json::from_str(
            r#"{"otp":{"subject":"Code","body":"{{.OTP}}"},"verification":null}"#,
        )
        .unwrap();
        assert_eq!(t.otp.subject, "Code");
        assert_eq!(t.password_reset, EmailTemplate::default());
        assert_eq!(t.verification, EmailTemplate::default());
        // A collection with no templates at all still gets the built-in ones.
        assert_eq!(
            EmailTemplates::default().password_reset.subject,
            "Reset your password"
        );
        let mut c: Collection = serde_json::from_str(r#"{"name":"users","type":"auth"}"#).unwrap();
        c.validate(None).unwrap();
        assert_eq!(c.email_templates, Some(EmailTemplates::default()));
    }

    #[test]
    fn a_field_without_a_type_is_refused() {
        for field in [
            r#"{"name":"x"}"#,
            r#"{"name":"x","type":""}"#,
            r#"{"name":"x","type":null}"#,
        ] {
            let mut c: Collection =
                serde_json::from_str(&format!(r#"{{"name":"notype","fields":[{field}]}}"#))
                    .unwrap();
            assert_eq!(
                c.validate(None).unwrap_err().0,
                "invalid type \"\" for field \"x\"",
                "{field}"
            );
        }
    }

    #[test]
    fn null_reads_as_the_zero_value() {
        let json = r#"{"id":null,"name":"a","type":"base","createdAt":null,"updatedAt":null,
            "rules":{"listRule":null,"viewRule":null,"createRule":null,"updateRule":null,
                     "deleteRule":null,"subscribeRule":null},
            "fields":[
              {"id":null,"name":"x","type":"text","required":null,"searchable":null,"unique":null},
              {"name":"r","type":"relation","relationConfig":
                {"targetMoul":null,"cardinality":null,"onDelete":null}}],
            "emailTemplates":{"otp":{"subject":null,"body":null}}}"#;
        let c: Collection = serde_json::from_str(json).unwrap();
        assert_eq!(c.id, "");
        assert_eq!(c.created_at, "");
        assert_eq!(c.rules, Rules::default());
        let x = &c.fields[0];
        assert!(x.id.is_empty() && !x.required && !x.searchable && !x.unique);
        assert_eq!(c.fields[1].relation, Some(Relation::default()));
        assert_eq!(c.email_templates.unwrap().otp, EmailTemplate::default());
        let c: Collection = serde_json::from_str(r#"{"name":null}"#).unwrap();
        assert_eq!(c.name, "");
    }

    #[test]
    fn options_and_relation_config_stay_on_other_types() {
        let rel = Relation {
            target_moul: "teams".into(),
            cardinality: "1:N".into(),
            on_delete: "CASCADE".into(),
        };
        let mut status = Field::new("status", FieldType::Text);
        status.options = vec!["a".into(), "b".into()];
        status.relation = Some(rel.clone());
        let mut c = Collection::new("posts", CollectionKind::Base);
        c.fields = vec![status];
        c.validate(None).unwrap();
        assert_eq!(c.fields[0].options, ["a", "b"]);
        assert_eq!(c.fields[0].relation, Some(rel));
        let back = c.to_json();
        assert_eq!(back["fields"][0]["options"], serde_json::json!(["a", "b"]));
        assert!(back["fields"][0].get("relationConfig").is_some());

        // Switching back to select works, because the options were kept.
        let previous = c.clone();
        c.fields[0].kind = FieldType::Select;
        c.validate(Some(&previous)).unwrap();
        assert_eq!(c.fields[0].options, ["a", "b"]);
    }

    #[test]
    fn field_ids_follow_names_then_renames() {
        let mut n = 0;
        let mut next = || {
            n += 1;
            format!("fld_{n}")
        };
        let mut v1 = Collection::new("posts", CollectionKind::Base);
        v1.fields = vec![
            Field::new("title", FieldType::Text),
            Field::new("body", FieldType::Text),
        ];
        assert_eq!(v1.match_field_ids(None, &mut next), ["fld_1", "fld_2"]);

        // Same names, no ids: matched by name. A new name gets a new id.
        let mut v2 = v1.clone();
        for f in &mut v2.fields {
            f.id.clear();
        }
        v2.fields.push(Field::new("tags", FieldType::Json));
        assert_eq!(v2.match_field_ids(Some(&v1), &mut next), ["fld_3"]);
        assert_eq!(v2.fields[0].id, "fld_1");

        // A rename keeps the id.
        let mut v3 = v2.clone();
        v3.fields[0].name = "headline".into();
        v3.fields.remove(1);
        let changes = FieldChanges::between(&v2, &v3);
        assert_eq!(
            changes.renamed,
            [(
                "fld_1".to_owned(),
                "title".to_owned(),
                "headline".to_owned()
            )]
        );
        assert_eq!(changes.removed, ["fld_2"]);
        assert!(changes.added.is_empty() && changes.changed.is_empty());
    }
}
