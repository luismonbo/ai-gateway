//! An upstream's Azure sign-in variables: `<NAME>_AUTH`, `<NAME>_CLIENT_ID`
//! and `<NAME>_TOKEN_SCOPE`.
//!
//! Like every config message, these never repeat a base URL or its host.

use super::{ConfigError, Env};
use reqwest::Url;
use std::fmt;
use std::net::IpAddr;

/// The scope Microsoft's current docs request tokens for.
pub const DEFAULT_TOKEN_SCOPE: &str = "https://ai.azure.com/.default";

/// Set by AKS's workload identity webhook in each pod that uses it.
const FEDERATED_TOKEN_FILE: &str = "AZURE_FEDERATED_TOKEN_FILE";

/// Hosts of Azure OpenAI and Foundry resources.
const AZURE_HOST_SUFFIXES: [&str; 3] = [
    ".openai.azure.com",
    ".services.ai.azure.com",
    ".cognitiveservices.azure.com",
];

/// How an upstream signs in to Azure, from `<NAME>_AUTH`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AzureSignIn {
    pub method: SignInMethod,
    /// What tokens are requested for, e.g. [`DEFAULT_TOKEN_SCOPE`].
    pub scope: String,
}

/// A client ID picks a user-assigned identity; `None` means the
/// system-assigned one (or, for workload identity, the pod's own).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SignInMethod {
    ManagedIdentity {
        client_id: Option<String>,
    },
    /// `managed_identity` on AKS, where the webhook set up a federated token.
    WorkloadIdentity {
        client_id: Option<String>,
    },
    AzureCli,
}

/// For the startup summary and logs. A client ID isn't a secret.
impl fmt::Display for SignInMethod {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            SignInMethod::ManagedIdentity { client_id: None } => {
                f.write_str("managed identity (system-assigned)")
            }
            SignInMethod::ManagedIdentity {
                client_id: Some(id),
            } => write!(f, "managed identity (client ID {id})"),
            SignInMethod::WorkloadIdentity { client_id: None } => f.write_str("workload identity"),
            SignInMethod::WorkloadIdentity {
                client_id: Some(id),
            } => write!(f, "workload identity (client ID {id})"),
            SignInMethod::AzureCli => f.write_str("azure cli"),
        }
    }
}

/// The variable names one upstream reads.
struct Names {
    auth: String,
    client_id: String,
    scope: String,
    base_url: String,
    api_key: String,
}

impl Names {
    fn new(prefix: &str) -> Self {
        Self {
            auth: format!("{prefix}_AUTH"),
            client_id: format!("{prefix}_CLIENT_ID"),
            scope: format!("{prefix}_TOKEN_SCOPE"),
            base_url: format!("{prefix}_BASE_URL"),
            api_key: format!("{prefix}_API_KEY"),
        }
    }
}

/// The upstream's sign-in settings, `None` without `<prefix>_AUTH`, and
/// their warnings. `url` is the upstream's parsed base URL.
pub(super) fn sign_in(
    env: &Env,
    prefix: &str,
    url: &Url,
    has_api_key: bool,
) -> Result<(Option<AzureSignIn>, Vec<String>), ConfigError> {
    let names = Names::new(prefix);
    let Some(auth) = env.get(&names.auth) else {
        return Ok((None, unused_warnings(env, &names)));
    };
    let method = match auth.to_ascii_lowercase().as_str() {
        "managed_identity" => managed_identity(env, &names)?,
        "azure_cli" => SignInMethod::AzureCli,
        // The value isn't repeated: it might be a key put in the wrong place.
        _ => {
            return Err(ConfigError::new(
                &names.auth,
                "must be managed_identity or azure_cli",
            ))
        }
    };
    if has_api_key {
        return Err(ConfigError::new(
            &names.auth,
            format!(
                "can't be set together with {}; an upstream either signs in or sends a key",
                names.api_key
            ),
        ));
    }
    let scope = token_scope(env, &names.scope)?;
    let client_id_warning =
        (method == SignInMethod::AzureCli && env.get(&names.client_id).is_some()).then(|| {
            format!(
                "{}: only used with {}=managed_identity; ignored",
                names.client_id, names.auth
            )
        });
    let host_warning = token_destination(url, &names)?;
    let warnings = client_id_warning.into_iter().chain(host_warning).collect();
    Ok((Some(AzureSignIn { method, scope }), warnings))
}

/// Without `<prefix>_AUTH`, the other sign-in variables do nothing. A shell
/// may export `AZURE_CLIENT_ID` for other tools, so they only warn.
fn unused_warnings(env: &Env, names: &Names) -> Vec<String> {
    [&names.client_id, &names.scope]
        .into_iter()
        .filter(|variable| env.get(variable).is_some())
        .map(|variable| format!("{variable}: ignored because {} is not set", names.auth))
        .collect()
}

/// Workload identity when AKS set it up, otherwise the platform's managed
/// identity; either may be user-assigned through `<prefix>_CLIENT_ID`.
fn managed_identity(env: &Env, names: &Names) -> Result<SignInMethod, ConfigError> {
    let client_id = env
        .get(&names.client_id)
        .map(|id| client_id(&names.client_id, id))
        .transpose()?;
    if env.get(FEDERATED_TOKEN_FILE).is_some() {
        Ok(SignInMethod::WorkloadIdentity { client_id })
    } else {
        Ok(SignInMethod::ManagedIdentity { client_id })
    }
}

/// A client ID is a GUID, kept in lowercase, its canonical form. The message
/// never repeats the value, which might be a client secret pasted into the
/// wrong variable.
fn client_id(variable: &str, value: String) -> Result<String, ConfigError> {
    if is_guid(&value) {
        return Ok(value.to_ascii_lowercase());
    }
    let message = if value.starts_with("/subscriptions/") {
        "looks like a resource ID; use the identity's client ID, a GUID such as \
         00000000-0000-0000-0000-000000000000"
    } else {
        "must be the client ID of a user-assigned managed identity, a GUID such as \
         00000000-0000-0000-0000-000000000000"
    };
    Err(ConfigError::new(variable, message))
}

/// 8-4-4-4-12 hexadecimal digits, in any letter case.
fn is_guid(value: &str) -> bool {
    const GROUPS: [usize; 5] = [8, 4, 4, 4, 12];
    value.split('-').count() == GROUPS.len()
        && value.split('-').zip(GROUPS).all(|(group, len)| {
            group.len() == len && group.bytes().all(|byte| byte.is_ascii_hexdigit())
        })
}

fn token_scope(env: &Env, variable: &str) -> Result<String, ConfigError> {
    let Some(scope) = env.get(variable) else {
        return Ok(DEFAULT_TOKEN_SCOPE.to_string());
    };
    match Url::parse(&scope) {
        Ok(url) if url.scheme() == "https" => Ok(scope),
        _ => Err(ConfigError::new(
            variable,
            format!("must be an https:// URL, such as {DEFAULT_TOKEN_SCOPE}"),
        )),
    }
}

/// Tokens never travel in clear text, except to this machine. A host that
/// isn't Azure's only warns: API Management and sovereign clouds use others.
fn token_destination(url: &Url, names: &Names) -> Result<Option<String>, ConfigError> {
    if is_loopback(url) {
        return Ok(None);
    }
    if url.scheme() != "https" {
        return Err(ConfigError::new(
            &names.base_url,
            format!(
                "must be an https:// URL when {} is set, so tokens never travel in clear \
                 text; http:// is allowed only for localhost and loopback addresses",
                names.auth
            ),
        ));
    }
    let is_azure = url.domain().is_some_and(|domain| {
        AZURE_HOST_SUFFIXES
            .iter()
            .any(|suffix| domain.ends_with(suffix))
    });
    Ok((!is_azure).then(|| {
        format!(
            "{}: the host isn't an Azure OpenAI or Foundry endpoint (*{}); {} will send \
             Azure tokens to it",
            names.base_url,
            AZURE_HOST_SUFFIXES.join(", *"),
            names.auth
        )
    }))
}

/// `localhost`, `127.0.0.0/8` or `::1`.
fn is_loopback(url: &Url) -> bool {
    match (url.domain(), url.host_str()) {
        (Some(domain), _) => domain == "localhost",
        // Not a domain, so an IP address; IPv6 ones are written in brackets.
        (None, Some(ip)) => ip
            .trim_start_matches('[')
            .trim_end_matches(']')
            .parse::<IpAddr>()
            .is_ok_and(|ip| ip.is_loopback()),
        (None, None) => false,
    }
}

#[cfg(test)]
mod tests;
