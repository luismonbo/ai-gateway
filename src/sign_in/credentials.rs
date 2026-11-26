//! The Microsoft credential behind each sign-in method.

use crate::config::SignInMethod;
use azure_core::credentials::TokenCredential;
use azure_core::http::{ClientOptions, HttpClient, Transport};
use azure_identity::{
    AzureCliCredential, AzureCliCredentialOptions, ClientAssertionCredentialOptions, Executor,
    ManagedIdentityCredential, ManagedIdentityCredentialOptions, UserAssignedId,
    WorkloadIdentityCredential, WorkloadIdentityCredentialOptions,
};
use std::sync::Arc;

/// The credential for `method`. Its token requests go through `transport`,
/// and the Azure CLI runs through `executor`.
///
/// Fails when this environment can't support `method`: a managed identity
/// on Azure Arc, Service Fabric, Cloud Shell or Azure ML, or workload
/// identity without `AZURE_TENANT_ID`.
pub fn credential(
    method: &SignInMethod,
    transport: Arc<dyn HttpClient>,
    executor: Arc<dyn Executor>,
) -> azure_core::Result<Arc<dyn TokenCredential>> {
    // Without a transport of ours, the crates would build their own HTTP
    // client, which the lean setup doesn't include.
    let client_options = ClientOptions {
        transport: Some(Transport::new(transport)),
        ..Default::default()
    };
    let credential: Arc<dyn TokenCredential> = match method {
        SignInMethod::ManagedIdentity { client_id } => {
            ManagedIdentityCredential::new(Some(ManagedIdentityCredentialOptions {
                user_assigned_id: client_id.clone().map(UserAssignedId::ClientId),
                client_options,
            }))?
        }
        SignInMethod::WorkloadIdentity { client_id } => {
            WorkloadIdentityCredential::new(Some(WorkloadIdentityCredentialOptions {
                credential_options: ClientAssertionCredentialOptions { client_options },
                client_id: client_id.clone(),
                ..Default::default()
            }))?
        }
        SignInMethod::AzureCli => AzureCliCredential::new(Some(AzureCliCredentialOptions {
            executor: Some(executor),
            ..Default::default()
        }))?,
    };
    Ok(credential)
}
