---
title: "Migrate to Transaction Stream Service"
---

This guide contains information on how to migrate to using the Transaction Stream Service if you are currently running a legacy indexer.

The old set up requires running an archival fullnode with additional threads to process the transactions which is difficult and expensive to maintain. Adding more custom logic either requires a bulkier machine, or running several fullnodes that scale linearly.

This new way of indexing requires only a single (or 2 for redundancy) fullnodes. Custom logic live in separate light servers that scale separately from the fullnodes so you don’t need to run multiple fullnodes. You can also directly stream from Labs provided endpoints which means that you no longer need to run fullnodes.

## 1. Clone the repo

```
# SSH
git clone git@github.com:aptos-labs/aptos-indexer-processors.git

# HTTPS
git clone https://github.com/aptos-labs/aptos-indexer-processors.git
```

Navigate to the directory for the service:

```
cd aptos-indexer-processors
cd rust/processor
```

## 2. Migrate processors to Transaction Stream Service

Follow the instructions in [Self-Hosted Indexer API](https://aptos.dev/indexer/api/self-hosted) to create a config file for each processor that you are migrating.

To connect your processor to the Transaction Stream Service, there are two options.

### Option a: Connect to Labs-provided Transaction Stream Service

By using the Labs-provided Transaction Stream Service, you no longer have to run an archival node. Instructions to connect to Labs-provided Transaction Stream can be found [here](https://aptos.dev/indexer/txn-stream/labs-hosted).

### Option b: Run a Self-Hosted Transaction Stream Service

If you choose to, you can run a self-hosted instance of the Transaction Stream Service and connect your processors to it. Instructions to run a Self-Hosted Transaction Stream can be found [here](https://aptos.dev/indexer/txn-stream/self-hosted).

## 3. (Optional) Migrate custom processors to Transaction Stream Service

If you have custom processors written with the old indexer, we highly recommend starting from scratch and using a new database than what your legacy indexer is already writing to. Using a new database ensures that all your custom database migrations will be applied during this migration.

### a. Migrate custom table schemas

Migrate your custom schemas by copying over each of your custom migrations to the [`migrations`](https://github.com/aptos-labs/aptos-indexer-processors/tree/main/rust/processor/migrations) folder.

### b. Migrate custom processors code

Migrate the code by copying over your custom processors to the [`processors`](https://github.com/aptos-labs/aptos-indexer-processors/tree/main/rust/processor) folder and any relevant custom models to the [`models`](https://github.com/aptos-labs/aptos-indexer-processors/tree/main/rust/processor/src/models) folder. Integrate the custom processors with the rest of the code by adding them to the following Rust code files.

[`mod.rs`](https://github.com/aptos-labs/aptos-indexer-processors/blob/main/rust/processor/src/processors/mod.rs)

```
pub enum Processor {
    ...
    CoinProcessor,
    ...
}

impl Processor {
    ...
    COIN_PROCESSOR_NAME => Self::CoinProcessor,
    ...
}
```

[`worker.rs`](https://github.com/aptos-labs/aptos-indexer-processors/blob/main/rust/processor/src/worker.rs)

```
Processor::CoinProcessor => {
    Arc::new(CoinTransactionProcessor::new(self.db_pool.clone()))
},
```

## 4. Backfill Postgres database with Diesel

Even though the new processors have the same Postgres schemas as the old ones, we recommend to do a complete backfill (ideally writing to a new DB altogether) because some fields are a bit different as a result of the protobuf conversion.

These instructions asusme you are familar with using [Diesel migrations](https://docs.rs/diesel_migrations/latest/diesel_migrations/). Run the full database migration with the following command:

```
DATABASE_URL=postgres://postgres@localhost:5432/postgres diesel migration run
```

## 5. Run the migrated processors

To run a single processor, use the following command:

```
cargo run --release -- -c config.yaml
```

If you have multiple processors, you'll need to run a separate instance of the service for each of the processors.

If you'd like to run the processor as a Docker image, the instructions are listed [here](https://aptos.dev/indexer/api/self-hosted#run-with-docker).

## FAQs

### 1. Will the protobuf ever be updated, and what do I need to do at that time?

The protobuf schema may be updated in the future. We will make sure communicate any incompatible changes.

### 2. What if I already have custom logic written in the old indexer? Is it easy to migrate those?

Since the new indexer stack has the same Postgres schema as the old indexer stack, it should be easy to migrate your processors. We still highly recommend creating a new DB for this migration so that any custom DB migrations are applie.

Follow Step 3 in this guide to migrate your custom logic over to the new processors stack.
