# Doppler Configuration for 5day4cast Development

This directory contains [Doppler](https://github.com/tee8z/doppler) scripts for setting up a local Bitcoin and Lightning Network environment for testing the 5day4cast coordinator.

## Prerequisites

1. Install Doppler:
```bash
git clone https://github.com/tee8z/doppler
cd doppler
cargo install --path .
```

2. Ensure you have Docker installed and running

## Network Topology

The scripts set up the following network:

```
┌─────────────┐
│   bitcoind  │
│    (bd1)    │
└──────┬──────┘
       │
   ┌───┴────┬─────────┬──────────┐
   │        │         │          │
┌──▼──┐  ┌─▼──┐   ┌──▼──┐   ┌───▼───┐
│alice│  │coord│   │ bob │   │esplora│
│(LND)│  │(LND)│   │(LND)│   │ (esp) │
└─────┘  └─────┘   └─────┘   └───────┘
```

## Scripts

### 1. config_nodes.doppler
Sets up the base infrastructure:
- Bitcoin Core node (`bd1`) in regtest mode
- Three LND nodes (`alice`, `coord`, `bob`)
- Esplora block explorer (`esp`)

### 2. make_channels.doppler
Funds the nodes and creates payment channels:
- Funds coordinator with 5,000,000 sats
- Creates bidirectional channels between all nodes
- Mines blocks to confirm transactions

### 3. mine_blocks.doppler
Continuously mines blocks every 5 seconds to simulate blockchain activity:

Notes

- The `SKIP_CONF` directive bypasses confirmation prompts for automated setup
- All nodes run in Bitcoin regtest mode for fast testing
- Channel amounts and topology can be modified in `make_channels.doppler`
- The coordinator node (`coord`) is configured to work with the 5day4cast server
