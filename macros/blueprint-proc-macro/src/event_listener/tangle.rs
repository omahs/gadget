use proc_macro2::TokenStream;
use quote::quote;
use syn::{Ident, LitInt};

#[allow(clippy::too_many_arguments)]
pub(crate) fn generate_tangle_event_handler(
    fn_name_string: &str,
    struct_name: &Ident,
    job_id: &LitInt,
    params_tokens: &[TokenStream],
    result_tokens: &[TokenStream],
    additional_params: &[TokenStream],
    fn_call: &TokenStream,
    event_listener_call: &TokenStream,
) -> TokenStream {
    quote! {
        /// Event handler for the function
        #[doc = "[`"]
        #[doc = #fn_name_string]
        #[doc = "`]"]
        pub struct #struct_name {
            pub service_id: u64,
            pub signer: gadget_sdk::keystore::TanglePairSigner<gadget_sdk::keystore::sp_core_subxt::sr25519::Pair>,
            #(#additional_params)*
        }

        #[automatically_derived]
        #[async_trait::async_trait]
        impl gadget_sdk::events_watcher::substrate::EventHandler<gadget_sdk::clients::tangle::runtime::TangleConfig> for #struct_name {
            async fn can_handle_events(
                &self,
                events: gadget_sdk::tangle_subxt::subxt::events::Events<gadget_sdk::clients::tangle::runtime::TangleConfig>,
            ) -> Result<bool, gadget_sdk::events_watcher::Error> {
                use gadget_sdk::tangle_subxt::tangle_testnet_runtime::api::services::events::JobCalled;

                static ONCE: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);
                if !ONCE.load(std::sync::atomic::Ordering::Relaxed) {
                    ONCE.store(true, std::sync::atomic::Ordering::Relaxed);
                    #event_listener_call
                }

                for evt in events.iter() {
                    if let Ok(evt) = evt {
                        ::gadget_sdk::info!("Event found || required: sid={}, jid={}", self.service_id, #job_id);
                    }
                }

                let has_event = events.find::<JobCalled>().flatten().any(|event| {
                    event.service_id == self.service_id && event.job == #job_id
                });

                Ok(has_event)
            }

            async fn handle_events(
                &self,
                client: gadget_sdk::tangle_subxt::subxt::OnlineClient<gadget_sdk::clients::tangle::runtime::TangleConfig>,
                (events, block_number): (
                    gadget_sdk::tangle_subxt::subxt::events::Events<gadget_sdk::clients::tangle::runtime::TangleConfig>,
                    u64
                ),
            ) -> Result<(), gadget_sdk::events_watcher::Error> {
                use gadget_sdk::tangle_subxt::{
                    subxt,
                    tangle_testnet_runtime::api::{
                        self as TangleApi,
                        runtime_types::{
                            bounded_collections::bounded_vec::BoundedVec,
                            tangle_primitives::services::field::{Field, BoundedString},
                        },
                        services::events::JobCalled,
                    },
                };

                ::gadget_sdk::info!("Handling actionable events ...");

                let job_events: Vec<_> = events
                    .find::<JobCalled>()
                    .flatten()
                    .filter(|event| {
                        event.service_id == self.service_id && event.job == #job_id
                    })
                    .collect();
                for call in job_events {
                    ::gadget_sdk::info!("Handling JobCalled Events: #{block_number}");

                    let mut args_iter = call.args.into_iter();
                    #(#params_tokens)*
                    #fn_call

                    let mut result = Vec::new();
                    #(#result_tokens)*

                    let response =
                        TangleApi::tx()
                            .services()
                            .submit_result(self.service_id, call.call_id, result);
                    gadget_sdk::tx::tangle::send(&client, &self.signer, &response).await?;
                }
                Ok(())
            }
        }
    }
}
