use core::{ops::RangeInclusive, time::Duration};
use std::{collections::BTreeMap, sync::Arc, vec::IntoIter};

use anyhow::{bail, ensure, Result};
use either::Either;
use enum_iterator::Sequence as _;
use ethereum_types::H64;
use execution_engine::{
    EngineGetPayloadV1Response, EngineGetPayloadV2Response, EngineGetPayloadV3Response,
    ExecutionPayloadV1, ExecutionPayloadV2, ExecutionPayloadV3, ForkChoiceStateV1,
    ForkChoiceUpdatedResponse, PayloadAttributes, PayloadId, PayloadStatusV1,
};
use futures::{channel::mpsc::UnboundedSender, lock::Mutex, Future};
use tracing::warn;
use prometheus_metrics::Metrics;
use reqwest::{header::HeaderMap, Client, Url};
use serde::{de::DeserializeOwned, Deserialize};
use serde_json::Value;
use static_assertions::const_assert_eq;
use std_ext::CopyExt;
use thiserror::Error;
use types::{
    combined::{ExecutionPayload, ExecutionPayloadParams},
    config::Config,
    nonstandard::{Phase, WithBlobsAndMev},
    phase0::primitives::{ExecutionBlockHash, ExecutionBlockNumber},
    preset::Preset,
};
use web3::{
    api::{Eth, Namespace as _},
    helpers::CallFuture,
    transports::Http,
    types::{BlockId, BlockNumber, FilterBuilder, U64},
    Error as Web3Error, Transport as _, Web3,
};

use crate::{
    auth::Auth, deposit_event::DepositEvent, eth1_block::Eth1Block, Eth1ApiToMetrics,
    Eth1ConnectionData,
};

const ENGINE_FORKCHOICE_UPDATED_TIMEOUT: Duration = Duration::from_secs(8);
const ENGINE_GET_PAYLOAD_TIMEOUT: Duration = Duration::from_secs(1);
const ENGINE_NEW_PAYLOAD_TIMEOUT: Duration = Duration::from_secs(8);

#[allow(clippy::struct_field_names)]
pub struct Eth1Api {
    config: Arc<Config>,
    client: Client,
    auth: Arc<Auth>,
    original: Vec<Url>,
    endpoints: Mutex<IntoIter<Url>>,
    eth1_api_to_metrics_tx: Option<UnboundedSender<Eth1ApiToMetrics>>,
    metrics: Option<Arc<Metrics>>,
}

impl Eth1Api {
    #[must_use]
    pub fn new(
        config: Arc<Config>,
        client: Client,
        auth: Arc<Auth>,
        eth1_rpc_urls: Vec<Url>,
        eth1_api_to_metrics_tx: Option<UnboundedSender<Eth1ApiToMetrics>>,
        metrics: Option<Arc<Metrics>>,
    ) -> Self {
        Self {
            config,
            client,
            auth,
            original: eth1_rpc_urls.clone(),
            endpoints: Mutex::new(eth1_rpc_urls.into_iter()),
            eth1_api_to_metrics_tx,
            metrics,
        }
    }

    pub async fn current_head_number(&self) -> Result<ExecutionBlockNumber> {
        Ok(self
            .request_with_fallback(|(api, headers)| Ok(api.block_number(headers)))
            .await?
            .as_u64())
    }

    pub async fn get_block(&self, block_id: BlockId) -> Result<Option<Eth1Block>> {
        self.request_with_fallback(|(api, headers)| Ok(api.block(block_id, headers)))
            .await?
            .map(Eth1Block::try_from)
            .transpose()
    }

    pub async fn get_block_by_number(
        &self,
        block_number: ExecutionBlockNumber,
    ) -> Result<Option<Eth1Block>> {
        self.get_block(U64::from(block_number).into()).await
    }

    pub async fn get_block_by_hash(
        &self,
        block_hash: ExecutionBlockHash,
    ) -> Result<Option<Eth1Block>> {
        self.get_block(block_hash.into()).await
    }

    pub async fn get_first_deposit_contract_block_number(
        &self,
    ) -> Result<Option<ExecutionBlockNumber>> {
        // `BlockNumber::Earliest` is necessary to get all logs.
        // `BlockNumber::Latest` is the default (in the JSON RPC, not in `web3`). See:
        // - <https://github.com/ethereum/wiki/wiki/JSON-RPC/b729c267fd71d9ba92ce6b90023caabc486ca5ae#eth_getlogs>
        // - <https://github.com/paritytech/wiki/blob/bc0952d26528de087993049fc72e4f6f003e688f/JSONRPC-eth-module.md#eth_newfilter>
        let filter = FilterBuilder::default()
            .from_block(BlockNumber::Earliest)
            .address(vec![self.config.deposit_contract_address])
            .limit(1)
            .build();

        let logs = self
            .request_with_fallback(|(api, headers)| Ok(api.logs(filter.clone(), headers)))
            .await?;

        if let Some(log) = logs.first() {
            if let Some(block_number) = log.block_number {
                return Ok(Some(block_number.as_u64()));
            }
        }

        Ok(None)
    }

    pub async fn get_blocks(
        &self,
        block_number_range: RangeInclusive<ExecutionBlockNumber>,
    ) -> Result<Vec<Eth1Block>> {
        let mut deposit_data = self.get_deposit_events(block_number_range.clone()).await?;
        let mut blocks = vec![];

        for block_number in block_number_range {
            match self.get_block_by_number(block_number).await? {
                Some(block) => {
                    let deposit_events = deposit_data.remove(&block_number).unwrap_or_default();
                    let eth1_block = Eth1Block {
                        deposit_events: deposit_events.try_into()?,
                        ..block
                    };
                    blocks.push(eth1_block);
                }
                None => continue,
            }
        }

        Ok(blocks)
    }

    pub async fn get_deposit_events(
        &self,
        block_number_range: RangeInclusive<ExecutionBlockNumber>,
    ) -> Result<BTreeMap<ExecutionBlockNumber, Vec<DepositEvent>>> {
        // Sepolia uses a custom contract that emits events other than `DepositEvent`. See:
        // - <https://github.com/ethereum/pm/issues/526>
        // - <https://github.com/protolambda/testnet-dep-contract/blob/8df70175dca186b74197ec830450c4b988861746/deposit_contract.sol>
        // - <https://notes.ethereum.org/zvkfSmYnT0-uxwwEegbCqg>
        // - <https://sepolia.etherscan.io/address/0x7f02C3E3c98b133055B8B348B2Ac625669Ed295D#events>
        // - <https://sepolia.etherscan.io/token/0x7f02C3E3c98b133055B8B348B2Ac625669Ed295D>
        let filter = FilterBuilder::default()
            .from_block(block_number_range.start().copy().into())
            .to_block(block_number_range.end().copy().into())
            .address(vec![self.config.deposit_contract_address])
            .topics(Some(vec![DepositEvent::TOPIC]), None, None, None)
            .build();

        let mut deposit_events = BTreeMap::<_, Vec<_>>::new();

        for log in self
            .request_with_fallback(|(api, headers)| Ok(api.logs(filter.clone(), headers)))
            .await?
        {
            let block_number = match log.block_number {
                Some(block_number) => block_number.as_u64(),
                None => continue,
            };

            let deposit_event = DepositEvent::try_from(log)?;

            deposit_events
                .entry(block_number)
                .or_default()
                .push(deposit_event);
        }

        Ok(deposit_events)
    }

    /// Calls [`engine_newPayloadV1`] or [`engine_newPayloadV2`] or [`engine_newPayloadV3`] depending on `payload`.
    ///
    /// Later versions of `engine_newPayload` accept parameters of all prior versions,
    /// but using the earlier versions allows the application to work with old execution clients.
    ///
    /// [`engine_newPayloadV1`]: https://github.com/ethereum/execution-apis/blob/b7c5d3420e00648f456744d121ffbd929862924d/src/engine/paris.md#engine_newpayloadv1
    /// [`engine_newPayloadV2`]: https://github.com/ethereum/execution-apis/blob/b7c5d3420e00648f456744d121ffbd929862924d/src/engine/shanghai.md#engine_newpayloadv2
    /// [`engine_newPayloadV3`]: https://github.com/ethereum/execution-apis/blob/a0d03086564ab1838b462befbc083f873dcf0c0f/src/engine/cancun.md#engine_newpayloadv3
    pub async fn new_payload<P: Preset>(
        &self,
        payload: ExecutionPayload<P>,
        params: Option<ExecutionPayloadParams>,
    ) -> Result<PayloadStatusV1> {
        match (payload, params) {
            (ExecutionPayload::Bellatrix(payload), None) => {
                let payload_v1 = ExecutionPayloadV1::from(payload);
                let params = vec![serde_json::to_value(payload_v1)?];
                self.execute(
                    "engine_newPayloadV1",
                    params,
                    Some(ENGINE_NEW_PAYLOAD_TIMEOUT),
                )
                .await
            }
            (ExecutionPayload::Capella(payload), None) => {
                let payload_v2 = ExecutionPayloadV2::from(payload);
                let params = vec![serde_json::to_value(payload_v2)?];
                self.execute(
                    "engine_newPayloadV2",
                    params,
                    Some(ENGINE_NEW_PAYLOAD_TIMEOUT),
                )
                .await
            }
            (
                ExecutionPayload::Deneb(payload),
                Some(ExecutionPayloadParams::Deneb {
                    versioned_hashes,
                    parent_beacon_block_root,
                }),
            ) => {
                let payload_v3 = ExecutionPayloadV3::from(payload);
                let params = vec![
                    serde_json::to_value(payload_v3)?,
                    serde_json::to_value(versioned_hashes)?,
                    serde_json::to_value(parent_beacon_block_root)?,
                ];
                self.execute(
                    "engine_newPayloadV3",
                    params,
                    Some(ENGINE_NEW_PAYLOAD_TIMEOUT),
                )
                .await
            }
            _ => bail!(Error::InvalidParameters),
        }
    }

    /// Calls [`engine_forkchoiceUpdatedV1`] or [`engine_forkchoiceUpdatedV2`] or [`engine_forkchoiceUpdatedV3`] depending on `payload_attributes`.
    ///
    /// Later versions of `engine_forkchoiceUpdated` accept parameters of all prior versions,
    /// but using the earlier versions allows the application to work with old execution clients.
    ///
    /// [`engine_forkchoiceUpdatedV1`]: https://github.com/ethereum/execution-apis/blob/b7c5d3420e00648f456744d121ffbd929862924d/src/engine/paris.md#engine_forkchoiceupdatedv1
    /// [`engine_forkchoiceUpdatedV2`]: https://github.com/ethereum/execution-apis/blob/b7c5d3420e00648f456744d121ffbd929862924d/src/engine/shanghai.md#engine_forkchoiceupdatedv2
    /// [`engine_forkchoiceUpdatedV3`]: https://github.com/ethereum/execution-apis/blob/a0d03086564ab1838b462befbc083f873dcf0c0f/src/engine/cancun.md#engine_forkchoiceupdatedv3
    pub async fn forkchoice_updated<P: Preset>(
        &self,
        head_block_hash: ExecutionBlockHash,
        safe_block_hash: ExecutionBlockHash,
        finalized_block_hash: ExecutionBlockHash,
        payload_attributes: Either<Phase, PayloadAttributes<P>>,
    ) -> Result<ForkChoiceUpdatedResponse> {
        let fork_choice_state = ForkChoiceStateV1 {
            head_block_hash,
            safe_block_hash,
            finalized_block_hash,
        };

        let phase = payload_attributes
            .as_ref()
            .either(CopyExt::copy, PayloadAttributes::phase);

        let payload_attributes = payload_attributes.right();

        let params = vec![
            serde_json::to_value(fork_choice_state)?,
            serde_json::to_value(payload_attributes)?,
        ];

        let RawForkChoiceUpdatedResponse {
            payload_id,
            payload_status,
        } = match phase {
            Phase::Bellatrix => {
                self.execute(
                    "engine_forkchoiceUpdatedV1",
                    params,
                    Some(ENGINE_FORKCHOICE_UPDATED_TIMEOUT),
                )
                .await?
            }
            Phase::Capella => {
                self.execute(
                    "engine_forkchoiceUpdatedV2",
                    params,
                    Some(ENGINE_FORKCHOICE_UPDATED_TIMEOUT),
                )
                .await?
            }
            Phase::Deneb => {
                self.execute(
                    "engine_forkchoiceUpdatedV3",
                    params,
                    Some(ENGINE_FORKCHOICE_UPDATED_TIMEOUT),
                )
                .await?
            }
            _ => {
                // This match arm will silently match any new phases.
                // Cause a compilation error if a new phase is added.
                const_assert_eq!(Phase::CARDINALITY, 5);

                bail!(Error::PhasePreBellatrix)
            }
        };

        let payload_id = match phase {
            Phase::Bellatrix => payload_id.map(PayloadId::Bellatrix),
            Phase::Capella => payload_id.map(PayloadId::Capella),
            Phase::Deneb => payload_id.map(PayloadId::Deneb),
            _ => {
                // This match arm will silently match any new phases.
                // Cause a compilation error if a new phase is added.
                const_assert_eq!(Phase::CARDINALITY, 5);

                bail!(Error::PhasePreBellatrix)
            }
        };

        Ok(ForkChoiceUpdatedResponse {
            payload_status,
            payload_id,
        })
    }

    /// Calls [`engine_getPayloadV1`] or [`engine_getPayloadV2`] or [`engine_getPayloadV3`]depending on `payload_id`.
    ///
    /// Newer versions of the method may be used to request payloads from all prior versions,
    /// but using the old methods allows the application to work with old execution clients.
    ///
    /// [`engine_getPayloadV1`]: https://github.com/ethereum/execution-apis/blob/b7c5d3420e00648f456744d121ffbd929862924d/src/engine/paris.md#engine_getpayloadv1
    /// [`engine_getPayloadV2`]: https://github.com/ethereum/execution-apis/blob/b7c5d3420e00648f456744d121ffbd929862924d/src/engine/shanghai.md#engine_getpayloadv2
    /// [`engine_getPayloadV3`]: https://github.com/ethereum/execution-apis/blob/a0d03086564ab1838b462befbc083f873dcf0c0f/src/engine/cancun.md#engine_getpayloadv3
    pub async fn get_payload<P: Preset>(
        &self,
        payload_id: PayloadId,
    ) -> Result<WithBlobsAndMev<ExecutionPayload<P>, P>> {
        match payload_id {
            PayloadId::Bellatrix(payload_id) => {
                let params = vec![serde_json::to_value(payload_id)?];

                self.execute::<EngineGetPayloadV1Response<P>>(
                    "engine_getPayloadV1",
                    params,
                    Some(ENGINE_GET_PAYLOAD_TIMEOUT),
                )
                .await
                .map(Into::into)
            }
            PayloadId::Capella(payload_id) => {
                let params = vec![serde_json::to_value(payload_id)?];

                self.execute::<EngineGetPayloadV2Response<P>>(
                    "engine_getPayloadV2",
                    params,
                    Some(ENGINE_GET_PAYLOAD_TIMEOUT),
                )
                .await
                .map(Into::into)
            }
            PayloadId::Deneb(payload_id) => {
                let params = vec![serde_json::to_value(payload_id)?];

                self.execute::<EngineGetPayloadV3Response<P>>(
                    "engine_getPayloadV3",
                    params,
                    Some(ENGINE_GET_PAYLOAD_TIMEOUT),
                )
                .await
                .map(Into::into)
            }
        }
    }

    async fn execute<T: DeserializeOwned + Send>(
        &self,
        method: &str,
        params: Vec<Value>,
        timeout: Option<Duration>,
    ) -> Result<T> {
        let _timer = self.metrics.as_ref().map(|metrics| {
            prometheus_metrics::start_timer_vec(&metrics.eth1_api_request_times, method)
        });

        self.request_with_fallback(|(api, headers)| {
            Ok(CallFuture::new(api.transport().execute_with_headers(
                method,
                params.clone(),
                headers,
                timeout,
            )))
        })
        .await
    }

    async fn request_with_fallback<R, O, F>(&self, request_from_api: R) -> Result<O>
    where
        R: Fn((Eth<Http>, Option<HeaderMap>)) -> Result<CallFuture<O, F>> + Sync + Send,
        O: DeserializeOwned + Send,
        F: Future<Output = Result<Value, Web3Error>> + Send,
    {
        while let Some(url) = self.current_endpoint().await {
            let http = Http::with_client(self.client.clone(), url.clone());
            let api = Web3::new(http).eth();
            let headers = self.auth.headers()?;
            let query = request_from_api((api, headers))?.await;

            match query {
                Ok(result) => {
                    if let Some(metrics_tx) = self.eth1_api_to_metrics_tx.as_ref() {
                        Eth1ApiToMetrics::Eth1Connection(Eth1ConnectionData {
                            sync_eth1_connected: true,
                            sync_eth1_fallback_connected: self.original.first() != Some(&url),
                        })
                        .send(metrics_tx);
                    }

                    return Ok(result);
                }
                Err(error) => {
                    if let Some(metrics) = self.metrics.as_ref() {
                        metrics.eth1_api_errors_count.inc();
                    }

                    match self.peek_next_endpoint().await {
                        Some(next_eth) => warn!(
                            "Eth1 RPC endpoint {url} returned an error: {error}; \
                             switching to {next_eth}",
                        ),
                        None => warn!(
                            "last available Eth1 RPC endpoint {url} returned an error: {error}",
                        ),
                    }

                    if let Some(metrics_tx) = self.eth1_api_to_metrics_tx.as_ref() {
                        Eth1ApiToMetrics::Eth1Connection(Eth1ConnectionData::default())
                            .send(metrics_tx);
                    }

                    self.next_endpoint().await;
                }
            }
        }

        self.reset_endpoints().await;

        if let Some(metrics) = self.metrics.as_ref() {
            metrics.eth1_api_reset_count.inc();
        }

        // Checking this in `Eth1Api::new` would be unnecessarily strict.
        // Syncing a predefined network without proposing blocks does not require an Eth1 RPC
        // (except during the Merge transition).
        ensure!(!self.original.is_empty(), Error::NoEndpointsProvided);

        bail!(Error::EndpointsExhausted)
    }

    async fn current_endpoint(&self) -> Option<Url> {
        self.endpoints.lock().await.as_slice().first().cloned()
    }

    async fn next_endpoint(&self) -> Option<Url> {
        self.endpoints.lock().await.next()
    }

    async fn peek_next_endpoint(&self) -> Option<Url> {
        self.endpoints.lock().await.as_slice().get(1).cloned()
    }

    async fn reset_endpoints(&self) {
        *self.endpoints.lock().await = self.original.clone().into_iter();
    }
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct RawForkChoiceUpdatedResponse {
    payload_status: PayloadStatusV1,
    payload_id: Option<H64>,
}

#[derive(Debug, Error)]
enum Error {
    #[error("all Eth1 RPC endpoints exhausted")]
    EndpointsExhausted,
    #[error("attempted to call Eth1 RPC endpoint with misconfigured parameters")]
    InvalidParameters,
    #[error("attempted to call Eth1 RPC endpoint but none were provided")]
    NoEndpointsProvided,
    #[error("pre-Bellatrix phase passed to Eth1Api::forkchoice_updated")]
    PhasePreBellatrix,
}

#[cfg(test)]
mod tests {
    use anyhow::Result;
    use execution_engine::PayloadValidationStatus;
    use hex_literal::hex;
    use httpmock::{Method, MockServer};
    use serde_json::json;
    use types::{
        bellatrix::containers::ExecutionPayload as BellatrixExecutionPayload,
        phase0::primitives::H256, preset::Mainnet,
    };

    use super::*;

    #[tokio::test]
    async fn test_bellatrix_payload_deserialization_with_real_response() -> Result<()> {
        let body = json!({
            "jsonrpc": "2.0",
            "id": 1,
            "result": {
                "parentHash": "0x2c7776c6c6c4a3fa2fbfc4886930e681fe4658e23e988b7ce27d4f355269b4a4",
                "feeRecipient": "0x0000000000000000000000000000000000000000",
                "stateRoot": "0xdeb98cee0497b499dc1a6a2323f990d350e80301fbbb0e778b62b5037fce5bf6",
                "receiptsRoot": "0x06215fe5ec9a1b418434561323471cc1c8cfc6ae121aaf03825596268581e098",
                "logsBloom": "0x00000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000",
                "prevRandao": "0x9ed233232634a35bbcb77b50cf357defc8f61ad5c74d5bba528e1a260f8d2f7f",
                "blockNumber": "0x7130a",
                "gasLimit": "0xf78798",
                "gasUsed": "0x14820",
                "timestamp": "0x621cc4f8",
                "extraData": "0xd883010a11846765746888676f312e31372e38856c696e7578",
                "baseFeePerGas": "0x7",
                "blockHash": "0xedd9cf26b9a0455a67e9abefe926796356ca6564d02463e229097c61ced696db",
                "transactions": [
                    "0xf86e078459682f0782520894419f2d6c3f5fe8bf43f91923ba21e996032897298894a1739b5e1d49c8808328d2f0a069dffffc6f9b20157bd17872d326de8ed088de3e24f2801dd9375ddbecd013f0a041aab6f5dff83fdd2595cc55725b28128b8902f12f3db598dce9f9183f989300",
                    "0x02f87883146966830516988459682f008459682f078252089432960b83199ae0f78756dbcf016a8e88e4dd7a748894a19041886f000080c001a0f916421115b1dc667b959fe32fa01cc9ba07942078b9e28435fd0a55c1cbf2dba076da1b6e79fa9a3b6b77e1601546fa194652a3f9a73919c470254833dfae68f8",
                    "0x02f87883146966830516998459682f008459682f0782520894b467d5ec9f6db8b1c156d40e65ebf88b2596ab198894a19041886f000080c001a0cc9ddcece6913c48e3aaaab25fb4f98da8540f1ffac58b010c9d3d0c60e01edba073cbf451658aa60dac89b62a463a2a95cdba3c73e5d258f80eedd6d465ab0772",
                    "0x02f878831469668305169a8459682f008459682f078252089477831a3a5552ad92848d7134a1e467c1089fb04a8894a19041886f000080c001a0f3687841790c73693a44710dfc83d02f7044ea821cca5a66b92b283c2c346d62a013695d07f9e62132a8c3c423dc4b74b914b5fa758f88fcf2ed10242aaa68ca6a",
                ],
            },
        });

        let server = MockServer::start();

        server.mock(|when, then| {
            when.method(Method::POST).path("/");
            then.status(200).body(body.to_string());
        });

        // The block seems to be from the Kintsugi testnet. There's no block explorer still serving
        // Kintsugi blocks to confirm it, but the block number and timestamp suggest that execution
        // layer genesis happened around 2021-12-15, just before the `MIN_GENESIS_TIME` of Kintsugi.
        let config = Arc::new(Config::mainnet());
        let auth = Arc::default();
        let server_url = server.url("/").parse()?;

        let eth1_api = Arc::new(Eth1Api::new(
            config,
            Client::new(),
            auth,
            vec![server_url],
            None,
            None,
        ));

        let payload_id = PayloadId::Bellatrix(H64(hex!("a5f7426cdca69a73")));

        eth1_api.get_payload::<Mainnet>(payload_id).await?;

        Ok(())
    }

    #[tokio::test]
    async fn test_capella_payload_deserialization_with_full_response() -> Result<()> {
        let body = json!({
            "jsonrpc": "2.0",
            "id": 0,
            "result": {
                "executionPayload": {
                    "parentHash": "0x98eff2712c5546167a22d9d3ab340005d8f736d49e8867ab2e67400526dc5d2c",
                    "feeRecipient": "0xe7cf7c3ba875dd3884ed6a9082d342cb4fbb1f1b",
                    "stateRoot": "0x54874eaadc381f61c2999a93c59c36e564a42062d64955e057991534fc166504",
                    "receiptsRoot": "0x56e81f171bcc55a6ff8345e692c0f86e5b48e01b996cadc001622fb5e363b421",
                    "logsBloom": "0x00000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000",
                    "prevRandao": "0x883fbdbbc4a4c75747422bc271c43bf6370f570c43cccd81f80cae71f54ad3da",
                    "blockNumber": "0x21b0",
                    "gasLimit": "0x1c9c380",
                    "gasUsed": "0x0",
                    "timestamp": "0x63d2af38",
                    "extraData": "0xd883010b00846765746888676f312e31392e35856c696e7578",
                    "baseFeePerGas": "0x7",
                    "blockHash": "0x1587569314611d9f06aac37c64c87b180313056d1a968e6b8290ce64c519859f",
                    "transactions": [
                        "0xf86e078459682f0782520894419f2d6c3f5fe8bf43f91923ba21e996032897298894a1739b5e1d49c8808328d2f0a069dffffc6f9b20157bd17872d326de8ed088de3e24f2801dd9375ddbecd013f0a041aab6f5dff83fdd2595cc55725b28128b8902f12f3db598dce9f9183f989300",
                        "0x02f87883146966830516988459682f008459682f078252089432960b83199ae0f78756dbcf016a8e88e4dd7a748894a19041886f000080c001a0f916421115b1dc667b959fe32fa01cc9ba07942078b9e28435fd0a55c1cbf2dba076da1b6e79fa9a3b6b77e1601546fa194652a3f9a73919c470254833dfae68f8",
                        "0x02f87883146966830516998459682f008459682f0782520894b467d5ec9f6db8b1c156d40e65ebf88b2596ab198894a19041886f000080c001a0cc9ddcece6913c48e3aaaab25fb4f98da8540f1ffac58b010c9d3d0c60e01edba073cbf451658aa60dac89b62a463a2a95cdba3c73e5d258f80eedd6d465ab0772",
                        "0x02f878831469668305169a8459682f008459682f078252089477831a3a5552ad92848d7134a1e467c1089fb04a8894a19041886f000080c001a0f3687841790c73693a44710dfc83d02f7044ea821cca5a66b92b283c2c346d62a013695d07f9e62132a8c3c423dc4b74b914b5fa758f88fcf2ed10242aaa68ca6a",
                    ],
                    "withdrawals": [
                        {
                            "index": "0x18561",
                            "validatorIndex": "0x7c2e8",
                            "address": "0xf97e180c050e5ab072211ad2c213eb5aee4df134",
                            "amount": "0x18111",
                        },
                        {
                            "index": "0x18562",
                            "validatorIndex": "0x7c2e9",
                            "address": "0xf97e180c050e5ab072211ad2c213eb5aee4df134",
                            "amount": "0x583a6",
                        },
                    ],
                },
                "blockValue": "0xffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffff",
            },
        });

        let server = MockServer::start();

        server.mock(|when, then| {
            when.method(Method::POST).path("/");
            then.status(200).body(body.to_string());
        });

        let config = Arc::new(Config::withdrawal_devnet_4());
        let auth = Arc::default();
        let server_url = server.url("/").parse()?;

        let eth1_api = Arc::new(Eth1Api::new(
            config,
            Client::new(),
            auth,
            vec![server_url],
            None,
            None,
        ));

        let payload_id = PayloadId::Capella(H64(hex!("a5f7426cdca69a73")));
        let payload = eth1_api.get_payload::<Mainnet>(payload_id).await?;

        assert_eq!(payload.value.phase(), Phase::Capella);

        Ok(())
    }

    #[tokio::test]
    async fn test_valid_payload_status_deserialization() -> Result<()> {
        let body = json!({
            "jsonrpc": "2.0",
            "id": 0,
            "result": {
                "status": "VALID",
                "latestValidHash": "0x0da76c72389ffe8b8bef1266213dd0dc4bf7030293913bfd69869cb349b13d35",
                "validationError": null,
            },
        });

        let server = MockServer::start();

        server.mock(|when, then| {
            when.method(Method::POST).path("/");
            then.status(200).body(body.to_string());
        });

        let config = Arc::new(Config::mainnet());
        let auth = Arc::default();
        let server_url = server.url("/").parse()?;

        let eth1_api = Arc::new(Eth1Api::new(
            config,
            Client::new(),
            auth,
            vec![server_url],
            None,
            None,
        ));

        let actual_status = eth1_api
            .new_payload::<Mainnet>(default_payload(), None)
            .await?;

        let expected_status = PayloadStatusV1 {
            status: PayloadValidationStatus::Valid,
            latest_valid_hash: Some(H256(hex!(
                "0da76c72389ffe8b8bef1266213dd0dc4bf7030293913bfd69869cb349b13d35"
            ))),
            validation_error: None,
        };

        assert_eq!(actual_status, expected_status);

        Ok(())
    }

    // `geth` responds to invalid payloads with objects containing `method` and `params`.
    // We had to fork `jsonrpc` because it does not allow nonstandard members.
    #[tokio::test]
    async fn test_invalid_payload_status_deserialization() -> Result<()> {
        let body = json!({
            "jsonrpc": "2.0",
            "method": "",
            "params": null,
            "id": 0,
            "result": {
                "latestValidHash": "0x5669a0cec34c19c288b9db210ea180d11ad3d92975234bdc769610b5fa4d7f80",
                "status": "INVALID",
                "validationError": null,
            },
        });

        let server = MockServer::start();

        server.mock(|when, then| {
            when.method(Method::POST).path("/");
            then.status(200).body(body.to_string());
        });

        let config = Arc::new(Config::mainnet());
        let auth = Arc::default();
        let server_url = server.url("/").parse()?;

        let eth1_api = Arc::new(Eth1Api::new(
            config,
            Client::new(),
            auth,
            vec![server_url],
            None,
            None,
        ));

        let actual_status = eth1_api
            .new_payload::<Mainnet>(default_payload(), None)
            .await?;

        let expected_status = PayloadStatusV1 {
            status: PayloadValidationStatus::Invalid,
            latest_valid_hash: Some(H256(hex!(
                "5669a0cec34c19c288b9db210ea180d11ad3d92975234bdc769610b5fa4d7f80"
            ))),
            validation_error: None,
        };

        assert_eq!(actual_status, expected_status);

        Ok(())
    }

    fn default_payload<P: Preset>() -> ExecutionPayload<P> {
        BellatrixExecutionPayload::default().into()
    }
}
