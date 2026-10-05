//! Render the discovery view from saved public oracle responses, without running a coordinator.
//! Usage: cargo run -p coordinator --example admin_preview -- eligible.json forecasts.json output-dir YYYY-MM-DD
use coordinator::{
    infra::{
        admin_weather::{Candidate, Discovery, EligibleStation, Filters, Forecast},
        refresh_cache::{Cached, Fetched},
    },
    templates::{
        admin::discovery::discovery,
        assets,
        layouts::admin::{admin_base, AdminPageConfig},
    },
};
use std::{path::PathBuf, sync::Arc};

fn main() -> anyhow::Result<()> {
    let args: Vec<_> = std::env::args().skip(1).collect();
    anyhow::ensure!(
        args.len() == 4,
        "expected eligible.json forecasts.json output-dir YYYY-MM-DD"
    );
    let stations: Vec<EligibleStation> = serde_json::from_slice(&std::fs::read(&args[0])?)?;
    let forecasts: Vec<Forecast> = serde_json::from_slice(&std::fs::read(&args[1])?)?;
    let candidates: Vec<_> = stations
        .into_iter()
        .filter_map(|eligible| {
            let rows: Vec<_> = forecasts
                .iter()
                .filter(|f| f.station_id == eligible.station.station_id)
                .collect();
            Some(Candidate {
                eligible,
                high: rows.iter().map(|f| f.temp_high).max()?,
                low: rows.iter().map(|f| f.temp_low).min()?,
                wind_knots: rows.iter().filter_map(|f| f.wind_speed).max(),
                rain_chance: rows.iter().filter_map(|f| f.precip_chance).max(),
                forecasts: rows.into_iter().cloned().collect(),
            })
        })
        .collect();
    let day = time::Date::parse(
        &args[3],
        &time::format_description::parse_borrowed::<2>("[year]-[month]-[day]")?,
    )?;
    let filters = Filters {
        day: args[3].clone(),
        ..Default::default()
    };
    let window = filters.window(day.midnight().assume_utc() - time::Duration::DAY)?;
    let data = Cached {
        latest: Some(Arc::new(Fetched::new(Discovery {
            eligible_count: candidates.len(),
            missing_forecasts: 0,
            candidates,
        }))),
        refreshing: false,
    };
    let content = maud::html! { div class="notification" { "Read-only layout preview · saved oracle data · creation is not connected" } (discovery(&filters, &window, &data, "signet")) };
    let page = admin_base(
        &AdminPageConfig {
            title: "Weather discovery preview",
            api_base: "",
            oracle_base: "",
            explorer_url: "",
            network: "signet",
            csrf_token: None,
        },
        content,
    )
    .into_string();
    let out = PathBuf::from(&args[2]);
    std::fs::create_dir_all(out.join("assets"))?;
    for asset in assets::ALL {
        if asset.content_type.starts_with("text/") {
            std::fs::write(out.join(asset.url.trim_start_matches('/')), asset.bytes)?;
        }
    }
    std::fs::write(out.join("index.html"), page)?;
    let script = bitcoin::ScriptBuf::from_bytes(hex::decode(
        "00141111111111111111111111111111111111111111",
    )?);
    let tx = bitcoin::Transaction {
        version: bitcoin::transaction::Version::TWO,
        lock_time: bitcoin::absolute::LockTime::ZERO,
        input: vec![bitcoin::TxIn {
            previous_output: bitcoin::OutPoint::new(
                "1111111111111111111111111111111111111111111111111111111111111111".parse()?,
                0,
            ),
            script_sig: bitcoin::ScriptBuf::new(),
            sequence: bitcoin::Sequence::ENABLE_RBF_NO_LOCKTIME,
            witness: bitcoin::Witness::new(),
        }],
        output: vec![
            bitcoin::TxOut {
                value: bitcoin::Amount::from_sat(12000),
                script_pubkey: script.clone(),
            },
            bitcoin::TxOut {
                value: bitcoin::Amount::from_sat(2500),
                script_pubkey: script.clone(),
            },
        ],
    };
    let mut psbt = bitcoin::Psbt::from_unsigned_tx(tx.clone())?;
    psbt.inputs[0].witness_utxo = Some(bitcoin::TxOut {
        value: bitcoin::Amount::from_sat(15000),
        script_pubkey: script,
    });
    let content = maud::html! { main.admin-workspace { p.eyebrow { "Layout fixture · synthetic transaction" } h1 { "Inspect the money" }
        (coordinator::templates::admin::transactions::transaction_diagram(&tx, "signet", "", "Funding", "Preview only: this transaction has not been broadcast", Some(&psbt.to_string()), None))
    } };
    let page = admin_base(
        &AdminPageConfig {
            title: "Funds preview",
            api_base: "",
            oracle_base: "",
            explorer_url: "",
            network: "signet",
            csrf_token: None,
        },
        content,
    )
    .into_string();
    std::fs::write(out.join("funds.html"), page)?;

    funds_preview(&out, &tx)?;
    wallet_preview(&out)?;
    Ok(())
}

fn funds_preview(out: &std::path::Path, tx: &bitcoin::Transaction) -> anyhow::Result<()> {
    use bitcoin::{
        hashes::{sha256, Hash},
        secp256k1::{Secp256k1, SecretKey},
    };
    use coordinator::domain::{
        admin_funds::*, ArkCommitment, Competition, CoordinatorFee, CreateEvent,
    };
    use lightning_invoice::{Currency, InvoiceBuilder, PaymentSecret};
    let invoice = |sats: u64, proof: u8| {
        InvoiceBuilder::new(Currency::Signet)
            .description("Synthetic support preview".into())
            .payment_hash(sha256::Hash::hash(&[proof; 32]))
            .payment_secret(PaymentSecret([8; 32]))
            .amount_milli_satoshis(sats * 1000)
            .duration_since_epoch(std::time::Duration::from_secs(1_800_000_000))
            .min_final_cltv_expiry_delta(18)
            .build_signed(|hash| {
                Secp256k1::new()
                    .sign_ecdsa_recoverable(hash, &SecretKey::from_slice(&[9; 32]).unwrap())
            })
            .unwrap()
            .to_string()
    };
    let now = time::OffsetDateTime::now_utc();
    let mut c = Competition::new(&CreateEvent {
        id: uuid::Uuid::from_u128(100),
        signing_date: now,
        start_observation_date: now - time::Duration::DAY,
        end_observation_date: now - time::Duration::HOUR,
        locations: vec!["KSEA".into(), "KPDX".into()],
        number_of_values_per_entry: 1,
        number_of_places_win: 1,
        total_allowed_entries: 2,
        entry_fee: 5000,
        coordinator_fee: CoordinatorFee::whole_percent(5),
        total_competition_pool: 10000,
        relative_locktime_block_delta: None,
        unlisted: false,
        scoring_rules: None,
        scoring_fields: None,
        max_entries_per_player: 1,
    });
    c.funding_transaction = Some(tx.clone());
    c.funding_outpoint = Some(bitcoin::OutPoint::new(tx.compute_txid(), 0));
    c.funding_confirmed_at = Some(now);
    let mut tickets = Vec::new();
    for index in 0..2 {
        let t = FundsTicket {
            ticket_id: uuid::Uuid::from_u128(101 + index).to_string(),
            competition_id: c.id.to_string(),
            entry_id: Some(uuid::Uuid::from_u128(201 + index).to_string()),
            payment_hash: format!("{:064x}", 300 + index),
            invoice: Some(invoice(6050, 1 + index as u8)),
            paid_at: Some("2026-10-02 11:00:00".into()),
            settled_at: Some("2026-10-02 11:00:02".into()),
            network_fee_sats: 750,
            escrow_address: Some(format!("ark1_synthetic_escrow_{index}")),
            swap_id: Some(uuid::Uuid::from_u128(301 + index).to_string()),
            vtxo: Some(format!("{:064x}:0", 400 + index)),
            escrow_sats: Some(6000),
            funded_at: Some(now.unix_timestamp()),
            refund_opens_at: Some(now.unix_timestamp() + 3600),
            escrow_pooled: true,
            ..Default::default()
        };
        let mut flow = TicketFlow {
            ticket: t,
            ..Default::default()
        };
        flow.payouts.push(FundsPayout {
            id: uuid::Uuid::from_u128(401 + index).to_string(),
            entry_id: flow.ticket.entry_id.clone().unwrap(),
            invoice: invoice(5000, 5 + index as u8),
            payment_hash: Some(format!("{:064x}", 500 + index)),
            amount_sats: 5000,
            initiated_at: "2026-10-02 12:00:00".into(),
            failed_at: Some("2026-10-02 12:00:01".into()),
            send_attempts: 2,
            lightning_address: Some("player@example.test".into()),
            ..Default::default()
        });
        if index == 0 {
            let mut paid = flow.payouts[0].clone();
            paid.id = uuid::Uuid::from_u128(501).to_string();
            paid.payment_hash = Some("b".repeat(64));
            paid.failed_at = None;
            paid.succeeded_at = Some("2026-10-02 12:01:00".into());
            flow.payouts.push(paid);
        } else {
            flow.payouts[0].failed_at = None;
            flow.payouts[0].next_send_at = Some(now.unix_timestamp() + 60);
        }
        tickets.push(flow);
    }
    let data = FundsPage {
        total: 2,
        tickets,
        commitment: Some(ArkCommitment {
            batch_id: "batch-preview".into(),
            commitment_tx: bitcoin::consensus::encode::serialize_hex(tx),
            funding_vout: 0,
        }),
    };
    let content = maud::html! {main.admin-workspace {p.eyebrow {"Synthetic support fixture · no real customer payments"} h1 {"Follow the money"} (coordinator::templates::admin::funds::funds_graph(&c,&data,"signet",""))}};
    let page = admin_base(
        &AdminPageConfig {
            title: "Funds support preview",
            api_base: "",
            oracle_base: "",
            explorer_url: "",
            network: "signet",
            csrf_token: None,
        },
        content,
    )
    .into_string();
    std::fs::write(out.join("customer-funds.html"), page)?;
    large_pools_preview(out, &c, &data, &invoice)?;
    let mut refund = data;
    c.funding_transaction = None;
    c.funding_outpoint = None;
    c.funding_confirmed_at = None;
    c.cancelled_at = Some(now);
    refund.commitment = None;
    for flow in &mut refund.tickets {
        flow.payouts.clear();
        flow.ticket.escrow_pooled = false;
        flow.ticket.refund_created_at = Some(now.unix_timestamp() - 90);
        flow.ticket.refund_updated_at = Some(now.unix_timestamp() - 20);
        flow.ticket.refund_opens_at = Some(now.unix_timestamp() - 120);
        flow.ticket.entry_id = None;
        flow.ticket.refund_id = Some(uuid::Uuid::now_v7().to_string());
        flow.ticket.refund_hash = Some("d".repeat(64));
        flow.ticket.refund_state = Some("submitted".into());
        flow.ticket.refund_invoice = Some(invoice(5950, 7));
        flow.ticket.refund_ark_txid = Some("e".repeat(64));
        flow.ticket.refund_fee_sats = Some(50);
    }
    refund.tickets[1].ticket.write_off =
        Some("Player registration never arrived; customer support review".into());
    let content = maud::html! {main.admin-workspace {p.eyebrow {"Synthetic refund fixture · no real customer payments"} h1 {"Refunds and recovery"} (coordinator::templates::admin::funds::funds_graph(&c,&refund,"signet",""))}};
    std::fs::write(
        out.join("customer-refunds.html"),
        admin_base(
            &AdminPageConfig {
                title: "Funds support preview",
                api_base: "",
                oracle_base: "",
                explorer_url: "",
                network: "signet",
                csrf_token: None,
            },
            content,
        )
        .into_string(),
    )?;
    let mut waiting = coordinator::domain::admin_funds::FundsPage {
        total: 3,
        tickets: refund.tickets.clone(),
        commitment: None,
    };
    waiting.tickets.push(waiting.tickets[0].clone());
    for (index, flow) in waiting.tickets.iter_mut().enumerate() {
        let t = &mut flow.ticket;
        t.ticket_id = uuid::Uuid::from_u128(700 + index as u128).to_string();
        t.entry_id = Some(uuid::Uuid::from_u128(800 + index as u128).to_string());
        t.refund_id = None;
        t.refund_state = None;
        t.refund_hash = None;
        t.refund_invoice = None;
        t.refund_ark_txid = None;
        t.refund_fee_sats = None;
        t.refund_created_at = None;
        t.refund_updated_at = None;
        t.write_off = None;
        t.refund_opens_at = match index {
            0 => Some(now.unix_timestamp() + 3600),
            1 => Some(now.unix_timestamp() - 120),
            _ => None,
        };
    }
    let content = maud::html! {main.admin-workspace {p.eyebrow {"Synthetic refund fixture · no real customer payments"} h1 {"Refunds before the worker starts"} (coordinator::templates::admin::funds::funds_graph(&c,&waiting,"signet",""))}};
    std::fs::write(
        out.join("refund-waiting.html"),
        admin_base(
            &AdminPageConfig {
                title: "Refund timing preview",
                api_base: "",
                oracle_base: "",
                explorer_url: "",
                network: "signet",
                csrf_token: None,
            },
            content,
        )
        .into_string(),
    )?;
    Ok(())
}

fn large_pools_preview(
    out: &std::path::Path,
    source: &coordinator::domain::Competition,
    sample: &coordinator::domain::admin_funds::FundsPage,
    invoice: &impl Fn(u64, u8) -> String,
) -> anyhow::Result<()> {
    use coordinator::domain::{admin_funds::*, CompetitionKind, PoolSummary, QueueSummary};
    use coordinator::templates::admin::funds::{funds_graph, pool_funds, pool_navigation};
    use uuid::Uuid;
    let mut parent = source.clone();
    parent.id = Uuid::from_u128(900);
    parent.kind = CompetitionKind::Queued;
    parent.queue = Some(QueueSummary {
        pool_rules: coordinator_escrow::pools::PoolRules::new(2, 25).unwrap(),
        entries: 75,
        max_entries: 500,
        stake_sats: 5000,
        terms_digest: "preview-terms".into(),
        pools: (0..3)
            .map(|index| PoolSummary {
                competition_id: Uuid::from_u128(901 + u128::from(index)),
                pool_index: index,
                players: 25,
            })
            .collect(),
    });
    let render = |content: maud::Markup| {
        let mut html = admin_base(
            &AdminPageConfig {
                title: "Funds support preview",
                api_base: "",
                oracle_base: "",
                explorer_url: "",
                network: "signet",
                csrf_token: None,
            },
            content,
        )
        .into_string();
        for index in 0..3 {
            let id = Uuid::from_u128(901 + index);
            // Keep this static preview's navigation local; live service links remain disconnected.
            for row in 0..25 {
                let ticket = Uuid::from_u128(10000 + index * 100 + row);
                html = html.replace(
                    &format!("/admin/funds?competition={id}&amp;ticket={ticket}"),
                    &format!("/pool-{}-entry-{}.html", index + 1, row + 1),
                );
            }
            html = html.replace(
                &format!("/admin/funds?competition={id}&amp;view=flow"),
                &format!("/pool-{}-flow.html", index + 1),
            );
            html = html.replace(
                &format!("/admin/funds?competition={id}"),
                &format!("/pool-{}.html", index + 1),
            );
        }
        html.replace(
            &format!("/admin/funds?competition={}", parent.id),
            "/competition-pools.html",
        )
    };
    std::fs::write(
        out.join("competition-pools.html"),
        render(maud::html! {
            main.admin-workspace {p.eyebrow {"Synthetic preview · 75 entries across three pools"} h1 {"KSEA · KPDX competition"}
                (pool_navigation(&parent,parent.id)) p.note {"Select a pool to follow its own contract and entries."}
            }
        }),
    )?;
    for index in 0..3u128 {
        let mut c = source.clone();
        c.id = Uuid::from_u128(901 + index);
        c.parent_id = Some(parent.id);
        c.pool_index = Some(index as u32);
        c.kind = CompetitionKind::Pool;
        c.event_submission.total_allowed_entries = 25;
        c.event_submission.total_competition_pool = 125000;
        let mut tx = c.funding_transaction.take().unwrap();
        tx.output[0].value = bitcoin::Amount::from_sat(125000);
        tx.lock_time = bitcoin::absolute::LockTime::from_consensus(index as u32);
        c.funding_outpoint = Some(bitcoin::OutPoint::new(tx.compute_txid(), 0));
        c.funding_transaction = Some(tx.clone());
        let mut tickets = Vec::new();
        for row in 0..25u128 {
            let mut flow = sample.tickets[0].clone();
            let id = Uuid::from_u128(10000 + index * 100 + row).to_string();
            flow.ticket.ticket_id = id.clone();
            flow.ticket.entry_id = Some(id.clone());
            flow.ticket.competition_id = c.id.to_string();
            flow.ticket.payment_hash = format!("{:064x}", 10000 + index * 100 + row);
            flow.ticket.invoice = Some(invoice(6050, (index * 25 + row + 1) as u8));
            flow.ticket.swap_id = Some(Uuid::from_u128(20000 + index * 100 + row).to_string());
            flow.ticket.vtxo = Some(format!("{:064x}:0", 30000 + index * 100 + row));
            flow.ticket.escrow_address =
                Some(format!("ark1_preview_pool_{}_entry_{}", index + 1, row + 1));
            flow.payouts.clear();
            if index == 1 && row == 0 {
                let mut payout = sample.tickets[0].payouts[0].clone();
                payout.entry_id = id;
                payout.id = Uuid::from_u128(40000).to_string();
                payout.amount_sats = 125000;
                payout.invoice = invoice(125000, 90);
                payout.payment_hash = Some("9".repeat(64));
                flow.payouts.push(payout.clone());
                payout.id = Uuid::from_u128(40001).to_string();
                payout.failed_at = None;
                payout.payment_hash = Some("8".repeat(64));
                payout.next_send_at = Some(1800000300);
                flow.payouts.push(payout);
            }
            if index == 2 {
                flow.ticket.escrow_pooled = false;
                flow.ticket.refund_opens_at =
                    Some(time::OffsetDateTime::now_utc().unix_timestamp() - 180);
                flow.ticket.refund_ark_txid = Some(format!("{:064x}", 60000 + row));
                flow.ticket.refund_created_at =
                    Some(time::OffsetDateTime::now_utc().unix_timestamp() - 120);
                flow.ticket.refund_updated_at =
                    Some(time::OffsetDateTime::now_utc().unix_timestamp() - 60);
                flow.ticket.refund_id = Some(Uuid::from_u128(50000 + row).to_string());
                flow.ticket.refund_hash = Some(format!("{:064x}", 50000 + row));
                flow.ticket.refund_state =
                    Some(if row < 23 { "settled" } else { "submitted" }.into());
                flow.ticket.refund_invoice = Some(invoice(5950, 100 + row as u8));
                if row == 24 {
                    flow.ticket.write_off = Some("Refund requires operator review".into());
                }
            }
            tickets.push(flow);
        }
        if index == 2 {
            c.funding_transaction = None;
            c.funding_outpoint = None;
            c.funding_confirmed_at = None;
            c.cancelled_at = Some(time::OffsetDateTime::now_utc());
        }
        let page = FundsPage {
            total: 25,
            tickets,
            commitment: None,
        };
        let heading = maud::html! {p.eyebrow {"Synthetic preview · no real payments"} h1 {"KSEA · KPDX competition"} (pool_navigation(&parent,c.id))};
        let content = maud::html! {main.admin-workspace {(heading.clone()) (pool_funds(&c,&page,"signet",""))}};
        std::fs::write(
            out.join(format!("pool-{}.html", index + 1)),
            render(content),
        )?;
        std::fs::write(
            out.join(format!("pool-{}-flow.html", index + 1)),
            render(
                maud::html! {main.admin-workspace {(heading.clone()) (funds_graph(&c,&page,"signet",""))}},
            ),
        )?;
        for row in 0..25 {
            let entry_page = FundsPage {
                total: 1,
                tickets: vec![page.tickets[row].clone()],
                commitment: None,
            };
            std::fs::write(
                out.join(format!("pool-{}-entry-{}.html", index + 1, row + 1)),
                render(
                    maud::html! {main.admin-workspace {(heading.clone()) (funds_graph(&c,&entry_page,"signet",""))}},
                ),
            )?;
        }
    }
    Ok(())
}

fn wallet_preview(out: &std::path::Path) -> anyhow::Result<()> {
    use coordinator::{
        domain::admin_wallet::WalletOverview,
        infra::{
            ark_swap::SwapWallet,
            bitcoin::WalletBalance,
            lightning::{ChannelBalance, NodeInfo},
        },
        templates::admin::wallet::wallet_page,
    };
    let data = WalletOverview {
        settled_address: None,
        settled: None,
        node: Some(serde_json::from_value::<NodeInfo>(
            serde_json::json!({"alias":"Example coordinator node", "identity_pubkey":"02abcdef0123456789abcdef0123456789abcdef0123456789abcdef012345678901", "synced_to_chain":true, "synced_to_graph":true, "num_active_channels":3, "num_inactive_channels":1,"num_pending_channels":0,"block_height":283000}),
        )?),
        channels: Some(serde_json::from_value::<ChannelBalance>(
            serde_json::json!({"local_balance":{"sat":"1250000"},"remote_balance":{"sat":"2800000"},"unsettled_local_balance":{"sat":"15000"}}),
        )?),
        onchain: Some(WalletBalance {
            confirmed: bitcoin::Amount::from_sat(500000),
            unconfirmed: bitcoin::Amount::from_sat(20000),
            locked: bitcoin::Amount::from_sat(150000),
        }),
        ark_configured: true,
        ark: Some(SwapWallet {
            payable_sat: Some(120000),
            expiring_sat: Some(25000),
            boarding_sat: Some(30000),
            recoverable_sat: Some(0),
            confirmed_sat: Some(100000),
            pre_confirmed_sat: Some(45000),
            earliest_expiry: Some(1791100000),
            boarding_address: Some("Preview only · verify the live address in ark-swapd".into()),
            ..Default::default()
        }),
    };
    for (name, data) in [
        ("wallet", data),
        (
            "wallet-unavailable",
            WalletOverview {
                ark_configured: true,
                ..Default::default()
            },
        ),
    ] {
        let page = admin_base(&AdminPageConfig { title: "Node & wallets preview", api_base:"", oracle_base:"", explorer_url:"", network:"signet", csrf_token:None }, maud::html! { p.notice { "Synthetic layout preview · balances are examples" } (wallet_page("signet", &data)) }).into_string();
        std::fs::write(out.join(format!("{name}.html")), page)?;
    }
    Ok(())
}
