use std::time::Duration;

use itertools::Itertools;

use tendermint_light_client::{
    components::{
        self,
        io::{AtHeight, Io, ProdIo},
    },
    light_client::LightClient as TmLightClient,
    state::State as LightClientState,
    store::{memory::MemoryStore, LightStore},
};
use tendermint_light_client_verifier::options::Options as TmOptions;
use tendermint_light_client_verifier::types::{Height as TMHeight, LightBlock, PeerId, Status};
use tendermint_light_client_verifier::ProdVerifier;
use tendermint_rpc as rpc;

use ibc_relayer_types::{
    clients::ics07_tendermint::{
        header::{headers_compatible, Header as TmHeader},
        misbehaviour::Misbehaviour as TmMisbehaviour,
    },
    core::{
        ics02_client::{client_type::ClientType, events::UpdateClient, header::downcast_header},
        ics24_host::identifier::ChainId,
    },
    downcast, Height as ICSHeight,
};
use tracing::trace;

use crate::{
    chain::cosmos::CosmosSdkChain, client_state::AnyClientState, config::ChainConfig, error::Error,
    misbehaviour::MisbehaviourEvidence,
};

use super::{genesis_restart::RestartAwareIo, Verified};

#[derive(Clone, Debug)]
enum AnyIo {
    Prod(ProdIo),
    RestartAware(RestartAwareIo),
}

impl Io for AnyIo {
    fn fetch_light_block(&self, height: AtHeight) -> Result<LightBlock, components::io::IoError> {
        match self {
            AnyIo::Prod(io) => io.fetch_light_block(height),
            AnyIo::RestartAware(io) => io.fetch_light_block(height),
        }
    }
}

pub struct LightClient {
    chain_id: ChainId,
    peer_id: PeerId,
    io: AnyIo,
}

impl super::LightClient<CosmosSdkChain> for LightClient {
    fn header_and_minimal_set(
        &mut self,
        trusted: ICSHeight,
        target: ICSHeight,
        client_state: &AnyClientState,
    ) -> Result<Verified<TmHeader>, Error> {
        let Verified { target, supporting } = self.verify(trusted, target, client_state)?;
        let (target, supporting) = self.adjust_headers(trusted, target, supporting)?;

        Ok(Verified { target, supporting })
    }

    fn verify(
        &mut self,
        trusted: ICSHeight,
        target: ICSHeight,
        client_state: &AnyClientState,
    ) -> Result<Verified<LightBlock>, Error> {
        trace!(%trusted, %target, "light client verification");

        let target_height =
            TMHeight::try_from(target.revision_height()).map_err(Error::invalid_height)?;

        let client = self.prepare_client(client_state)?;
        let mut state = self.prepare_state(trusted)?;

        // Verify the target header
        let target = client
            .verify_to_target(target_height, &mut state)
            .map_err(|e| Error::light_client_verification(self.chain_id.to_string(), e))?;

        // Collect the verification trace for the target block
        let target_trace = state.get_trace(target.height());

        // Compute the minimal supporting set, sorted by ascending height
        let supporting = target_trace
            .into_iter()
            .filter(|lb| lb.height() != target.height())
            .unique_by(LightBlock::height)
            .sorted_by_key(LightBlock::height)
            .collect_vec();

        Ok(Verified { target, supporting })
    }

    fn fetch(&mut self, height: ICSHeight) -> Result<LightBlock, Error> {
        trace!(%height, "fetching header");

        let height = TMHeight::try_from(height.revision_height()).map_err(Error::invalid_height)?;
        self.fetch_light_block(AtHeight::At(height))
    }

    /// Given a client update event that includes the header used in a client update,
    /// look for misbehaviour by fetching a header at same or latest height.
    ///
    /// ## TODO
    /// - [ ] Return intermediate headers as well
    fn check_misbehaviour(
        &mut self,
        update: &UpdateClient,
        client_state: &AnyClientState,
    ) -> Result<Option<MisbehaviourEvidence>, Error> {
        crate::time!("light client check_misbehaviour");

        let update_header = update.header.clone().ok_or_else(|| {
            Error::misbehaviour(format!(
                "missing header in update client event {}",
                self.chain_id
            ))
        })?;

        let update_header: &TmHeader =
            downcast_header(update_header.as_ref()).ok_or_else(|| {
                Error::misbehaviour(format!(
                    "header type incompatible for chain {}",
                    self.chain_id
                ))
            })?;

        let latest_chain_block = self.fetch_light_block(AtHeight::Highest)?;
        let latest_chain_height =
            ICSHeight::new(self.chain_id.version(), latest_chain_block.height().into())
                .map_err(|_| Error::invalid_height_no_source())?;

        // set the target height to the minimum between the update height and latest chain height
        let target_height = core::cmp::min(update.consensus_height(), latest_chain_height);
        let trusted_height = update_header.trusted_height;

        // TODO - check that a consensus state at trusted_height still exists on-chain,
        // currently we don't have access to Cosmos chain query from here

        if trusted_height >= latest_chain_height {
            // Can happen with multiple FLA attacks, we return no evidence and hope to catch this in
            // the next iteration. e.g:
            // existing consensus states: 1000, 900, 300, 200 (only known by the caller)
            // latest_chain_height = 300
            // target_height = 1000
            // trusted_height = 900
            return Ok(None);
        }

        let Verified { target, supporting } =
            self.verify(trusted_height, target_height, client_state)?;

        if !headers_compatible(&target.signed_header, &update_header.signed_header) {
            let (witness, supporting) = self.adjust_headers(trusted_height, target, supporting)?;

            let misbehaviour = TmMisbehaviour {
                client_id: update.client_id().clone(),
                header1: update_header.clone(),
                header2: witness,
            }
            .into();

            Ok(Some(MisbehaviourEvidence {
                misbehaviour,
                supporting_headers: supporting.into_iter().map(Into::into).collect(),
            }))
        } else {
            Ok(None)
        }
    }
}

fn io_for_addr(
    addr: &rpc::Url,
    peer_id: PeerId,
    timeout: Option<Duration>,
) -> Result<ProdIo, Error> {
    let rpc_client = rpc::HttpClient::new(addr.clone()).map_err(|e| Error::rpc(addr.clone(), e))?;
    Ok(ProdIo::new(peer_id, rpc_client, timeout))
}

impl LightClient {
    pub fn from_config(config: &ChainConfig, peer_id: PeerId) -> Result<Self, Error> {
        let live_io = io_for_addr(&config.rpc_addr, peer_id, Some(config.rpc_timeout))?;

        let io = match &config.genesis_restart {
            None => AnyIo::Prod(live_io),
            Some(genesis_restart) => {
                let archive_io = io_for_addr(
                    &genesis_restart.archive_addr,
                    peer_id,
                    Some(config.rpc_timeout),
                )?;

                AnyIo::RestartAware(RestartAwareIo::new(
                    genesis_restart.restart_height,
                    live_io,
                    archive_io,
                ))
            }
        };

        Ok(Self {
            chain_id: config.id.clone(),
            peer_id,
            io,
        })
    }

    fn prepare_client(&self, client_state: &AnyClientState) -> Result<TmLightClient, Error> {
        let clock = components::clock::SystemClock;
        let verifier = ProdVerifier::default();
        let scheduler = components::scheduler::basic_bisecting_schedule;

        let client_state =
            downcast!(client_state => AnyClientState::Tendermint).ok_or_else(|| {
                Error::client_type_mismatch(ClientType::Tendermint, client_state.client_type())
            })?;

        let params = TmOptions {
            trust_threshold: client_state
                .trust_threshold
                .try_into()
                .map_err(Error::light_client_state)?,
            trusting_period: client_state.trusting_period,
            clock_drift: client_state.max_clock_drift,
        };

        Ok(TmLightClient::new(
            self.peer_id,
            params,
            clock,
            scheduler,
            verifier,
            self.io.clone(),
        ))
    }

    fn prepare_state(&self, trusted: ICSHeight) -> Result<LightClientState, Error> {
        let trusted_height =
            TMHeight::try_from(trusted.revision_height()).map_err(Error::invalid_height)?;

        let trusted_block = self.fetch_light_block(AtHeight::At(trusted_height))?;

        let mut store = MemoryStore::new();
        store.insert(trusted_block, Status::Trusted);

        Ok(LightClientState::new(store))
    }

    fn fetch_light_block(&self, height: AtHeight) -> Result<LightBlock, Error> {
        self.io
            .fetch_light_block(height)
            .map_err(|e| Error::light_client_io(self.chain_id.to_string(), e))
    }

    fn adjust_headers(
        &mut self,
        trusted_height: ICSHeight,
        target: LightBlock,
        supporting: Vec<LightBlock>,
    ) -> Result<(TmHeader, Vec<TmHeader>), Error> {
        use super::LightClient;

        trace!(
            trusted = %trusted_height, target = %target.height(),
            "adjusting headers with {} supporting headers", supporting.len()
        );

        // Get the light block at trusted_height + 1 from chain.
        //
        // NOTE: This is needed to get the next validator set. While there is a next validator set
        //       in the light block at trusted height, the proposer is not known/set in this set.
        let trusted_validator_set = self.fetch(trusted_height.increment())?.validators;

        let mut supporting_headers = Vec::with_capacity(supporting.len());

        let mut current_trusted_height = trusted_height;
        let mut current_trusted_validators = trusted_validator_set.clone();

        for support in supporting {
            let header = TmHeader {
                signed_header: support.signed_header.clone(),
                validator_set: support.validators,
                trusted_height: current_trusted_height,
                trusted_validator_set: current_trusted_validators,
            };

            // This header is now considered to be the currently trusted header
            current_trusted_height = header.height();

            // Therefore we can now trust the next validator set, see NOTE above.
            current_trusted_validators = self.fetch(header.height().increment())?.validators;

            supporting_headers.push(header);
        }

        // a) Set the trusted height of the target header to the height of the previous
        // supporting header if any, or to the initial trusting height otherwise.
        //
        // b) Set the trusted validators of the target header to the validators of the successor to
        // the last supporting header if any, or to the initial trusted validators otherwise.
        let (latest_trusted_height, latest_trusted_validator_set) = match supporting_headers.last()
        {
            Some(prev_header) => {
                let prev_succ = self.fetch(prev_header.height().increment())?;
                (prev_header.height(), prev_succ.validators)
            }
            None => (trusted_height, trusted_validator_set),
        };

        let target_header = TmHeader {
            signed_header: target.signed_header,
            validator_set: target.validators,
            trusted_height: latest_trusted_height,
            trusted_validator_set: latest_trusted_validator_set,
        };

        Ok((target_header, supporting_headers))
    }
}
