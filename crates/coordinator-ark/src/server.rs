//! The Arkade server: its parameters, and the escrows it will accept.

use ark_core::server::{GetVtxosRequest, Info, VirtualTxOutPoint};
use bitcoin::absolute::LockTime;
use bitcoin::{Network, Psbt, Txid, XOnlyPublicKey};
use coordinator_ark_escrow::{
    EntryEscrow, EscrowTerms, RelativeTimelock, ServerRules, MAINNET_HRP, TESTNET_HRP,
};

use crate::Error;

/// What the server returns for a submitted offchain spend: its own signatures on the Ark
/// transaction, and the checkpoints still waiting for the owner's.
pub struct OffchainSubmission {
    pub ark_tx: Psbt,
    pub checkpoints: Vec<Psbt>,
}

/// A connected Arkade server and its `/v1/info`.
#[derive(Clone)]
pub struct ArkServer {
    client: ark_grpc::Client,
    info: Info,
    rules: ServerRules,
}

impl ArkServer {
    /// Connect to `url`, such as `https://mutinynet.arkade.sh`, and read the server's parameters.
    pub async fn connect(url: impl Into<String>) -> Result<Self, Error> {
        // tonic's TLS uses rustls's process-wide crypto provider.
        // This workspace compiles in both ring and aws-lc-rs, so rustls cannot pick one itself.
        // Install ring, unless the process has already chosen.
        let _ = rustls::crypto::ring::default_provider().install_default();
        let mut client = ark_grpc::Client::new(url.into());
        client.connect().await?;
        let info = client.get_info().await?;
        let rules = server_rules(&info)?;
        Ok(Self {
            client,
            info,
            rules,
        })
    }

    pub fn client(&self) -> &ark_grpc::Client {
        &self.client
    }

    pub fn info(&self) -> &Info {
        &self.info
    }

    pub fn rules(&self) -> &ServerRules {
        &self.rules
    }

    /// The Ark address prefix on this server's network.
    pub fn hrp(&self) -> &'static str {
        address_hrp(self.info.network)
    }

    /// Terms for an entry escrow created at `created_at`, refundable from `refund_at`.
    ///
    /// Both times are UNIX seconds.
    /// See [`escrow_terms`].
    pub fn escrow_terms(
        &self,
        player: XOnlyPublicKey,
        coordinator: XOnlyPublicKey,
        refund_at: u32,
        created_at: u32,
    ) -> Result<EscrowTerms, Error> {
        escrow_terms(&self.rules, player, coordinator, refund_at, created_at)
    }

    /// Submit an offchain spend for the server to co-sign, leaving it pending.
    ///
    /// The owner signs the Ark transaction first, because the server's signatures come back with
    /// this call; the checkpoints it returns are then signed and handed to [`Self::finalize_offchain`].
    pub async fn submit_offchain(
        &self,
        ark_tx: Psbt,
        checkpoints: Vec<Psbt>,
    ) -> Result<OffchainSubmission, Error> {
        let response = self
            .client
            .submit_offchain_transaction_request(ark_tx, checkpoints)
            .await?;
        Ok(OffchainSubmission {
            ark_tx: response.signed_ark_tx,
            checkpoints: response.signed_checkpoint_txs,
        })
    }

    /// Finalize a submitted spend, once its checkpoints carry every signature.
    pub async fn finalize_offchain(
        &self,
        ark_txid: Txid,
        checkpoints: Vec<Psbt>,
    ) -> Result<(), Error> {
        self.client
            .finalize_offchain_transaction(ark_txid, checkpoints)
            .await?;
        Ok(())
    }

    /// Build an escrow and confirm this server will accept it in a batch.
    pub fn entry_escrow(&self, terms: EscrowTerms) -> Result<EntryEscrow, Error> {
        let escrow = EntryEscrow::new(terms)?;
        self.rules.check(escrow.vtxo_script())?;
        Ok(escrow)
    }

    /// Every VTXO at the escrows' addresses, including spent ones.
    pub async fn escrow_vtxos(
        &self,
        escrows: &[EntryEscrow],
    ) -> Result<Vec<VirtualTxOutPoint>, Error> {
        let addresses = escrows.iter().map(|escrow| {
            ark_core::ArkAddress::new(
                self.info.network,
                self.rules.signer,
                escrow.vtxo_script().output_key(),
            )
        });
        let response = self
            .client
            .list_vtxos(GetVtxosRequest::new_for_addresses(addresses))
            .await?;
        Ok(response.vtxos)
    }
}

/// What `info`'s server accepts in a VTXO script.
///
/// `/v1/info` does not say whether the server allows block-based timelocks.
/// A server whose own exit delay is in blocks must allow them.
/// Otherwise this assumes it does not, since timestamps and second-based delays are accepted either way.
pub fn server_rules(info: &Info) -> Result<ServerRules, Error> {
    let min_exit_delay = RelativeTimelock::from_sequence(info.unilateral_exit_delay)?;
    Ok(ServerRules {
        signer: info.signer_pk.x_only_public_key().0,
        min_exit_delay,
        block_timelocks_allowed: matches!(min_exit_delay, RelativeTimelock::Blocks(_)),
    })
}

/// Terms for an entry escrow on a server with `rules`.
///
/// - The refund locktime `T` is the timestamp `refund_at`.
/// - The exit delay is the server's minimum, which must be in seconds.
/// - The unilateral refund delay ends after `T` plus the exit delay, counted from `created_at`.
pub fn escrow_terms(
    rules: &ServerRules,
    player: XOnlyPublicKey,
    coordinator: XOnlyPublicKey,
    refund_at: u32,
    created_at: u32,
) -> Result<EscrowTerms, Error> {
    let refund_locktime = LockTime::from_time(refund_at)
        .map_err(|error| Error::InvalidPool(format!("refund time {refund_at}: {error}")))?;
    if !matches!(rules.min_exit_delay, RelativeTimelock::Seconds(_)) {
        return Err(Error::ServerInfo(
            "escrows need a server whose exit delay is in seconds".into(),
        ));
    }
    let exit_delay = rules.min_exit_delay;
    let unilateral_refund_delay =
        EscrowTerms::unilateral_refund_delay_for(refund_locktime, exit_delay, created_at)?;
    Ok(EscrowTerms {
        player,
        coordinator,
        server: rules.signer,
        refund_locktime,
        exit_delay,
        unilateral_refund_delay,
    })
}

/// The Ark address prefix for `network`: `ark` on mainnet, `tark` elsewhere.
pub fn address_hrp(network: Network) -> &'static str {
    match network {
        Network::Bitcoin => MAINNET_HRP,
        _ => TESTNET_HRP,
    }
}
