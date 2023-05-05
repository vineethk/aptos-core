/// This module provides a veiled coin type, denoted `VeiledCoin<T>` that hides the value/denomination of a coin.
/// Importantly, although veiled transactions hide the amount of coins sent they still leak the sender and recipient.
///
/// ## Terminology
///
/// 1. *Veiled coin*: a coin whose value is secret; i.e., it is encrypted under the owner's public key
///
/// 2. *Veiled amount*: any amount that is secret; i.e., encrypted under some public key
///
/// 3. *Veiled transaction*: a transaction that hides its amount transferred; i.e., a transaction whose amount is veiled
///
/// 4. *Veiled balance*: unlike a normal balance, a veiled balance is secret; i.e., it is encrypted under the account's
///    public key
///
/// ## Limitations
///
/// **WARNING:** This module is **experimental**! It is *NOT* production-ready. Specifically:
///
///  1. deploying this module will likely lead to lost funds
///  2. this module has not been cryptographically-audited
///  3. the current implementation is vulnerable to _front-running attacks_ as described in the Zether paper [BAZB20].
///  4. there is no integration with wallet software which, for veiled accounts, must maintain an additional ElGamal
///    encryption keypair
///  5. there is no support for rotating the ElGamal encryption public key of a veiled account
///
/// ### Veiled coin amounts as truncated `u32`'s
///
/// Veiled coin amounts must be specified as `u32`'s rather than `u64`'s as would be typical for normal coins in the
/// Aptos framework. This is because coin amounts must be encrypted with an *efficient*, additively-homomorphic encryption
/// scheme. Currently, our best candidate is ElGamal encryption in the exponent, which can only decrypt values around
/// 32 bits or slightly larger.
///
/// Specifically, veiled coins are the middle 32 bits of the normal 64 bit coin values. In order to convert a `u32`
/// veiled coin amount to a normal `u64` coin amount, we have to shift it left by 16 bits.
///
/// ```
///   u64 normal coin amount format:
///   [ left    || middle  || right ]
///   [ 63 - 32 || 31 - 16 || 15 - 0]
///
///   u32 veiled coin amount format; we take the middle 32 bits from the `u64` format above and store them in a `u32`:
///   [ middle ]
///   [ 31 - 0 ]
/// ```
///
/// Recall that: A coin has a *decimal precision* $d$ (e.g., for `AptosCoin`, $d = 8$; see `initialize` in
/// `aptos_coin.move`). This precision $d$ is used when displaying a `u64` amount, by dividing the amount by $10^d$.
/// For example, if the precision $d = 2$, then a `u64` amount of 505 coins displays as 5.05 coins.
///
/// For veield coins, we can easily display a `u32` `Coin<T>` amount $v$ by:
///  1. Casting $v$ as a u64 and shifting this left by 16 bits, obtaining a 64-bit $v'$
///  2. Displaying $v'$ normally, by dividing it by $d$, which is the precision in `CoinInfo<T>`.
///
/// ## Implementation details
///
/// This module leverages a so-called "resource account," which helps us mint a `VeiledCoin<T>` from a
/// normal `coin::Coin<T>` by transferring this latter coin into a `coin::CoinStore<T>` stored in the
/// resource account.
///
/// Later on, when someone wants to convert their `VeiledCoin<T>` into a normal `coin::Coin<T>`,
/// the resource account can be used to transfer out the normal from its coin store. Transfering out a coin like this
/// requires a `signer` for the resource account, which the `veiled_coin` module can obtain via a `SignerCapability`.
///
/// ## References
///
/// [BAZB20] Zether: Towards Privacy in a Smart Contract World; by Bunz, Benedikt and Agrawal, Shashank and Zamani,
/// Mahdi and Boneh, Dan; in Financial Cryptography and Data Security; 2020
module veiled_coin::veiled_coin {
    use std::error;
    use std::signer;
    use std::vector;
    use std::option::Option;

    use aptos_std::elgamal::Self;
    use aptos_std::ristretto255::{Self, RistrettoPoint, Scalar};

    use aptos_framework::account;
    use aptos_framework::coin::{Self, Coin};
    use aptos_framework::event::{Self, EventHandle};
    use aptos_std::bulletproofs::RangeProof;
    use aptos_std::bulletproofs;

    #[test_only]
    use std::features;
    #[test_only]
    use std::string::utf8;
    #[test_only]
    use aptos_std::debug::print;

    //
    // Errors
    //

    /// The range proof system does not support proofs for any number \in [0, 2^{32})
    const ERANGE_PROOF_SYSTEM_HAS_INSUFFICIENT_RANGE : u64 = 1;

    /// A range proof failed to verify.
    const ERANGE_PROOF_VERIFICATION_FAILED : u64 = 2;

    /// Account already has `VeiledCoinStore<CoinType>` registered.
    const EVEILED_COIN_STORE_ALREADY_PUBLISHED: u64 = 3;

    /// Account hasn't registered `VeiledCoinStore<CoinType>`.
    const EVEILED_COIN_STORE_NOT_PUBLISHED: u64 = 4;

    /// Not enough coins to complete transaction.
    const EINSUFFICIENT_BALANCE: u64 = 5;

    /// Failed deserializing bytes into either ElGamal ciphertext or $\Sigma$-protocol proof.
    const EDESERIALIZATION_FAILED: u64 = 6;

    /// Byte vector given for deserialization was the wrong length.
    const EBYTES_WRONG_LENGTH: u64 = 7;

    /// $\Sigma$-protocol proof for withdrawals did not verify.
    const ESIGMA_PROTOCOL_VERIFY_FAILED: u64 = 8;

    /// Tried cutting out more elements than are in the vector via `cut_vector`.
    const EVECTOR_CUT_TOO_LARGE: u64 = 9;

    /// The `NUM_LEAST_SIGNIFICANT_BITS_REMOVED` and `NUM_MOST_SIGNIFICANT_BITS_REMOVED` constants need to sum to 32 (bits).
    const EU64_COIN_AMOUNT_CLAMPING_IS_INCORRECT: u64 = 10;

    /// Non-specific internal error (see source code)
    const EINTERNAL_ERROR: u64 = 11;

    //
    // Constants
    //

    /// The maximum number of bits used to represent a coin's value.
    const MAX_BITS_IN_VALUE : u64 = 32;

    /// When converting a `u64` normal (public) amount to a `u32` veiled amount, we keep the middle 32 bits and
    /// remove the `NUM_LEAST_SIGNIFICANT_BITS_REMOVED` least significant bits and the `NUM_MOST_SIGNIFICANT_BITS_REMOVED`
    /// most significant bits (see comments in the beginning of this file).
    ///
    /// When converting a `u32` veiled amount to a `u64` normal (public) amount, we simply cast it to `u64` and shift it
    /// left by `NUM_LEAST_SIGNIFICANT_BITS_REMOVED`.
    const NUM_LEAST_SIGNIFICANT_BITS_REMOVED: u8 = 16;

    /// See `NUM_LEAST_SIGNIFICANT_BITS_REMOVED` comments.
    const NUM_MOST_SIGNIFICANT_BITS_REMOVED: u8 = 16;

    /// The domain separation tag (DST) used for the Bulletproofs prover.
    const VEILED_COIN_DST : vector<u8> = b"AptosVeiledCoin/BulletproofRangeProof";

    /// The domain separation tag (DST) used in the Fiat-Shamir transform of our $\Sigma$-protocol.
    const FIAT_SHAMIR_SIGMA_DST : vector<u8> = b"AptosVeiledCoin/WithdrawalProofFiatShamir";

    //
    // Structs
    //

    /// Main structure representing a coin in an account's custody.
    struct VeiledCoin<phantom CoinType> {
        /// ElGamal ciphertext which encrypts the number of coins $v \in [0, 2^{32})$. This $[0, 2^{32})$ range invariant
        /// is enforced throughout the code via Bulletproof-based ZK range proofs.
        veiled_amount: elgamal::Ciphertext,
    }

    /// A holder of a specific coin type and its associated event handles.
    /// These are kept in a single resource to ensure locality of data.
    struct VeiledCoinStore<phantom CoinType> has key {
        /// A ElGamal ciphertext of a value $v \in [0, 2^{32})$, an invariant that is enforced throughout the code.
        veiled_balance: elgamal::CompressedCiphertext,
        deposit_events: EventHandle<DepositEvent>,
        withdraw_events: EventHandle<WithdrawEvent>,
        pk: elgamal::CompressedPubkey,
    }

    /// Holds an `account::SignerCapability` for the resource account created when initializing this module. This
    /// resource account houses a `coin::CoinStore<T>` for every type of coin `T` that is veiled.
    struct VeiledCoinMinter has store, key {
        signer_cap: account::SignerCapability,
    }

    /// A cryptographic proof that ensures correctness of a veiled-to-veiled coin transfer.
    struct VeiledTransferProof<phantom CoinType> has drop {
        new_balance_proof: RangeProof,
        veiled_amount_proof: RangeProof,
        sigma_proof: FullSigmaProof<CoinType>,
    }

    /// A cryptographic proof that ensures correctness of a veiled-to-*unveiled* coin transfer.
    struct UnveiledWithdrawalProof<phantom CoinType> has drop {
        sigma_proof: ElGamalToPedSigmaProof<CoinType>,
        new_balance_proof: RangeProof,
    }

    /// A $\Sigma$-protocol proof used as part of a `VeiledTransferProof`.
    /// This proof encompasses the $\Sigma$-protocol from `ElGamalToPedSigmaProof`.
    /// (A more detailed description can be found in `verify_withdrawal_sigma_protocol`.)
    struct FullSigmaProof<phantom CoinType> has drop {
        x1: RistrettoPoint,
        x3: RistrettoPoint,
        x4: RistrettoPoint,
        alpha1: Scalar,
        alpha3: Scalar,
    }

    /// A $\Sigma$-protocol proof used as part of a `UnveiledWithdrawalProof`.
    /// (A more detailed description can be found in TODO: implement.)
    struct ElGamalToPedSigmaProof<phantom CoinType> has drop {
        // TODO: implement
    }

    /// Event emitted when some amount of veiled coins were deposited into an account.
    struct DepositEvent has drop, store {
        // We cannot leak any information about how much has been deposited.
    }

    /// Event emitted when some amount of veiled coins were withdrawn from an account.
    struct WithdrawEvent has drop, store {
        // We cannot leak any information about how much has been withdrawn.
    }

    //
    // Module initialization, done only once when this module is first published on the blockchain
    //

    /// Initializes a so-called "resource" account which will maintain a `coin::CoinStore<T>` resource for all `Coin<T>`'s
    /// that have been converted into a `VeiledCoin<T>`.
    fun init_module(deployer: &signer) {
        assert!(
            bulletproofs::get_max_range_bits() >= MAX_BITS_IN_VALUE,
            error::internal(ERANGE_PROOF_SYSTEM_HAS_INSUFFICIENT_RANGE)
        );

        assert!(
            NUM_LEAST_SIGNIFICANT_BITS_REMOVED + NUM_MOST_SIGNIFICANT_BITS_REMOVED == 32,
            error::internal(EU64_COIN_AMOUNT_CLAMPING_IS_INCORRECT)
        );

        // Create the resource account. This will allow this module to later obtain a `signer` for this account and
        // transfer `Coin<T>`'s into its `CoinStore<T>` before minting a `VeiledCoin<T>`.
        let (_resource, signer_cap) = account::create_resource_account(deployer, vector::empty());

        move_to(deployer,
            VeiledCoinMinter {
                signer_cap
            }
        )
    }

    //
    // Entry functions
    //

    /// Initializes a veiled coin store for the specified `user` account with that user's ElGamal encryption public key.
    /// Importantly, the user's wallet must retain their corresponding secret key.
    public entry fun register<CoinType>(user: &signer, pk: vector<u8>) {
        let pk = elgamal::new_pubkey_from_bytes(pk);
        register_internal<CoinType>(user, std::option::extract(&mut pk));
    }

    /// Sends a *public* `amount` of normal coins from `sender` to the `recipient`'s veiled balance.
    ///
    /// **WARNING:** This function *leaks* the transferred `amount`, since it is given as a public input.
    public entry fun veil_to<CoinType>(
        sender: &signer, recipient: address, amount: u32) acquires VeiledCoinMinter, VeiledCoinStore
    {
        let c = coin::withdraw<CoinType>(sender, cast_u32_to_u64_amount(amount));

        let vc = veiled_mint_from_coin(c);

        veiled_deposit<CoinType>(recipient, vc)
    }

    /// Like `veil_to` except the `sender` is also the recipient.
    ///
    /// This function can be used by the `sender` to initialize his veiled balance to a *public* value.
    ///
    /// **WARNING:** The initialized balance is *leaked*, since its initialized `amount` is public here.
    public entry fun veil<CoinType>(sender: &signer, amount: u32) acquires VeiledCoinMinter, VeiledCoinStore {
        veil_to<CoinType>(sender, signer::address_of(sender), amount)
    }

    /// Takes a *public* `amount` of `VeiledCoin<CoinType>` coins from `sender`, unwraps them to a `coin::Coin<CoinType>`,
    /// and sends them to `recipient`. Maintains secrecy of `sender`'s new balance.
    ///
    /// Requires a range proof on the new balance of the sender, to ensure the sender has enough money to send.
    /// No range proof is necessary for the `amount`, which is given as a public `u32` value.
    ///
    /// **WARNING:** This *leaks* the transferred `amount`, since it is a public `u32` argument.
    public entry fun unveil_to<CoinType>(
        sender: &signer,
        recipient: address,
        amount: u32,
        range_proof_new_balance: vector<u8>) acquires VeiledCoinStore, VeiledCoinMinter
    {
        let range_proof_new_balance = bulletproofs::range_proof_from_bytes(range_proof_new_balance);

        let c = unveiled_withdraw<CoinType>(
            sender,
            amount,
            &range_proof_new_balance);

        coin::deposit<CoinType>(recipient, c);
    }

    /// Like `unveil_to`, except the `sender` is also the recipient.
    public entry fun unveil<CoinType>(
        sender: &signer,
        amount: u32,
        range_proof_new_balance: vector<u8>) acquires VeiledCoinStore, VeiledCoinMinter
    {
        unveil_to<CoinType>(sender, signer::address_of(sender), amount, range_proof_new_balance)
    }

    /// Sends a *veiled* `amount` from `sender` to `recipient`. After this call, the balance of the `sender`
    /// and `recipient` remains (or becomes) secret.
    ///
    /// The sent amount remains secret; It is encrypted both under the sender's PK (in `withdraw_ct`) and under the
    /// recipient's PK (in `deposit_ct`) using the *same* ElGamal randomness.
    ///
    /// Requires a `VeiledTransferProof`; i.e.:
    /// 1. A range proof on the new balance of the sender, to ensure the sender has enough money to send (in
    ///    `range_proof_new_balance`),
    /// 2. A range proof on the transferred amount in `withdraw_ct`, to ensure the sender won't create coins out of thin
    ///    air (in `range_proof_veiled_amount`),
    /// 3. A $\Sigma$-protocol to prove that 'veiled_withdraw_amount' encrypts the same veiled amount as
    ///    'veiled_deposit_amount' with the same randomness (in `sigma_proof_bytes`).
    public entry fun fully_veiled_transfer<CoinType>(
        sender: &signer,
        recipient: address,
        withdraw_ct: vector<u8>,
        deposit_ct: vector<u8>,
        range_proof_new_balance: vector<u8>,
        range_proof_veiled_amount: vector<u8>,
        sigma_proof_bytes: vector<u8>) acquires VeiledCoinStore
    {
        let veiled_withdraw_amount = elgamal::new_ciphertext_from_bytes(withdraw_ct);
        assert!(std::option::is_some(&veiled_withdraw_amount), error::invalid_argument(EDESERIALIZATION_FAILED));

        let veiled_deposit_amount = elgamal::new_ciphertext_from_bytes(deposit_ct);
        assert!(std::option::is_some(&veiled_deposit_amount), error::invalid_argument(EDESERIALIZATION_FAILED));

        // This $\Sigma$-protocol proofs proves that `veiled_withdraw_amount` encrypts the same value using the same
        // randomness as `veiled_deposit_amount` (i.e., the amount being transferred). These two ciphertexts are
        // required as we need to update both the sender's and the recipient's balances, which use different public keys
        // and so must be updated with ciphertexts encrypted under their respective public keys.
        let sigma_proof = deserialize_sigma_proof<CoinType>(sigma_proof_bytes);
        assert!(std::option::is_some(&sigma_proof), error::invalid_argument(EDESERIALIZATION_FAILED));

        // Requires a range proof on the new balance of the sender, to ensure the sender has enough money to send, in
        // addition to a  range proof on the transferred amount.
        let new_balance_proof = bulletproofs::range_proof_from_bytes(range_proof_new_balance);
        let veiled_amount_proof = bulletproofs::range_proof_from_bytes(range_proof_veiled_amount);

        let transfer_proof = VeiledTransferProof {
            new_balance_proof,
            veiled_amount_proof,
            sigma_proof: std::option::extract(&mut sigma_proof)
        };

        fully_veiled_transfer_internal<CoinType>(
            sender,
            recipient,
            std::option::extract(&mut veiled_withdraw_amount),
            std::option::extract(&mut veiled_deposit_amount),
            &transfer_proof,
        )
    }

    //
    // Public functions.
    //

    /// Clamps a `u64` normal public amount to a `u32` to-be-veiled amount.
    ///
    /// WARNING: Precision is lost here (see "Veiled coin amounts as truncated `u32`'s" in the top-level comments)
    ///
    /// (Unclear if this function will be needed.)
    public fun clamp_u64_to_u32_amount(amount: u64): u32 {
        // Removes the `NUM_MOST_SIGNIFICANT_BITS_REMOVED` most significant bits.
        amount << NUM_MOST_SIGNIFICANT_BITS_REMOVED;
        amount >> NUM_MOST_SIGNIFICANT_BITS_REMOVED;

        // Removes the other `32 - NUM_MOST_SIGNIFICANT_BITS_REMOVED` least significant bits.
        amount = amount >> NUM_LEAST_SIGNIFICANT_BITS_REMOVED;

        // We are now left with a 32-bit value
        (amount as u32)
    }

    /// Casts a `u32` to-be-veiled amount to a `u64` normal public amount. No precision is lost here.
    public fun cast_u32_to_u64_amount(amount: u32): u64 {
        (amount as u64) << NUM_MOST_SIGNIFICANT_BITS_REMOVED
    }

    /// Returns `true` if `addr` is registered to receive veiled coins of `CoinType`.
    public fun has_veiled_coin_store<CoinType>(addr: address): bool {
        exists<VeiledCoinStore<CoinType>>(addr)
    }

    /// Returns the ElGamal encryption of the value of `coin`.
    public fun veiled_amount<CoinType>(coin: &VeiledCoin<CoinType>): &elgamal::Ciphertext {
        &coin.veiled_amount
    }

    /// Returns the ElGamal encryption of the veiled balance of `owner` for the provided `CoinType`.
    public fun veiled_balance<CoinType>(owner: address): elgamal::CompressedCiphertext acquires VeiledCoinStore {
        assert!(
            has_veiled_coin_store<CoinType>(owner),
            error::not_found(EVEILED_COIN_STORE_NOT_PUBLISHED),
        );

        borrow_global<VeiledCoinStore<CoinType>>(owner).veiled_balance
    }

    /// Given an address `addr`, returns the ElGamal encryption public key associated with that address
    public fun encryption_public_key<CoinType>(addr: address): elgamal::CompressedPubkey acquires VeiledCoinStore {
        assert!(
            has_veiled_coin_store<CoinType>(addr),
            error::not_found(EVEILED_COIN_STORE_NOT_PUBLISHED)
        );

        borrow_global_mut<VeiledCoinStore<CoinType>>(addr).pk
    }

    /// Returns the total supply of veiled coins
    public fun total_veiled_coins<CoinType>(): u64 acquires VeiledCoinMinter {
        let rsrc_acc_addr = signer::address_of(&get_resource_account_signer());
        assert!(coin::is_account_registered<CoinType>(rsrc_acc_addr), EINTERNAL_ERROR);

        coin::balance<CoinType>(rsrc_acc_addr)
    }

    /// Like `register`, but the public key is parsed in an `elgamal::CompressedPubkey` struct.
    /// TODO: Do we want to require a PoK of the SK here?
    public fun register_internal<CoinType>(user: &signer, pk: elgamal::CompressedPubkey) {
        let account_addr = signer::address_of(user);
        assert!(
            !has_veiled_coin_store<CoinType>(account_addr),
            error::already_exists(EVEILED_COIN_STORE_ALREADY_PUBLISHED),
        );

        // Note: There is no way to find an ElGamal SK such that the `(0_G, 0_G)` ciphertext below decrypts to a non-zero
        // value. We'd need to have `(r * G, v * G + r * pk) = (0_G, 0_G)`, which implies `r = 0` for any choice of PK/SK.
        // Thus, we must have `v * G = 0_G`, which implies `v = 0`.

        let coin_store = VeiledCoinStore<CoinType> {
            veiled_balance: elgamal::ciphertext_from_compressed_points(
                ristretto255::point_identity_compressed(), ristretto255::point_identity_compressed()),
            deposit_events: account::new_event_handle<DepositEvent>(user),
            withdraw_events: account::new_event_handle<WithdrawEvent>(user),
            pk,
        };
        move_to(user, coin_store);
    }

    /// Mints a veiled coin from a normal coin, shelving the normal coin into the resource account's coin store.
    ///
    /// **WARNING:** Fundamentally, there is no way to hide the value of the coin being minted here.
    public fun veiled_mint_from_coin<CoinType>(c: Coin<CoinType>): VeiledCoin<CoinType> acquires VeiledCoinMinter {
        // If there is no CoinStore<CoinType> in the resource account, create one.
        let rsrc_acc_signer = get_resource_account_signer();
        let rsrc_acc_addr = signer::address_of(&rsrc_acc_signer);
        if (!coin::is_account_registered<CoinType>(rsrc_acc_addr)) {
            coin::register<CoinType>(&rsrc_acc_signer);
        };

        // Move the normal coin into the coin store, so we can mint a veiled coin.
        // (There is no other way to drop a normal coin, for safety reasons, so moving it into a coin store is
        //  the only option.)
        let value_u64 = coin::value(&c);
        let value_u32 = clamp_u64_to_u32_amount(value_u64);
        let value = ristretto255::new_scalar_from_u32(
            value_u32
        );

        // Paranoid check: assert that the u64 coin value had only its middle 32 bits set
        assert!(cast_u32_to_u64_amount(value_u32) == value_u64, error::internal(EINTERNAL_ERROR));

        coin::deposit(rsrc_acc_addr, c);

        VeiledCoin<CoinType> {
            veiled_amount: elgamal::new_ciphertext_no_randomness(&value)
        }
    }

    /// Removes a *public* `amount` of veiled coins from `sender` and returns them as a normal `coin::Coin`.
    ///
    /// Requires a ZK range proof on the new balance of the `sender`, to ensure the `sender` has enough money to send.
    /// Since the `amount` is public, no ZK range proof on it is required.
    ///
    /// **WARNING:** This function *leaks* the public `amount`.
    public fun unveiled_withdraw<CoinType>(
        sender: &signer,
        amount: u32,
        new_balance_proof: &RangeProof): Coin<CoinType> acquires VeiledCoinStore, VeiledCoinMinter
    {
        let addr = signer::address_of(sender);

        assert!(has_veiled_coin_store<CoinType>(addr), error::not_found(EVEILED_COIN_STORE_NOT_PUBLISHED));

        let scalar_amount = ristretto255::new_scalar_from_u32(amount);
        let veiled_amount = elgamal::new_ciphertext_no_randomness(&scalar_amount);

        let coin_store = borrow_global_mut<VeiledCoinStore<CoinType>>(addr);

        // Since `veiled_amount` was created from a `u32` public `amount`, no ZK range proof is needed for it.
        veiled_withdraw(veiled_amount, coin_store, new_balance_proof, &std::option::none());

        // Note: If the above `withdraw` aborts, the whole TXN aborts, so there are no atomicity issues.
        coin::withdraw(&get_resource_account_signer(), cast_u32_to_u64_amount(amount))
    }


    /// Like `fully_veiled_transfer`, except the ciphertext and proofs have been deserialized into their respective structs.
    public fun fully_veiled_transfer_internal<CoinType>(
        sender: &signer,
        recipient_addr: address,
        veiled_withdraw_amount: elgamal::Ciphertext,
        veiled_deposit_amount: elgamal::Ciphertext,
        transfer_proof: &VeiledTransferProof<CoinType>) acquires VeiledCoinStore
    {
        let sender_addr = signer::address_of(sender);

        let sender_pk = encryption_public_key<CoinType>(sender_addr);
        let recipient_pk = encryption_public_key<CoinType>(recipient_addr);

        // Note: The `get_pk_from_addr` call from above already asserts that `sender_addr` has a coin store.
        let sender_coin_store = borrow_global_mut<VeiledCoinStore<CoinType>>(sender_addr);

        // Checks that `veiled_withdraw_amount` and `veiled_deposit_amount` encrypt the same amount of coins, under the
        // sender and recipient's PKs, respectively, by verifying the $\Sigma$-protocol proof in `transfer_proof`.
        // TODO: full $\Sigma$-protocol verify (need to fetch sender's balance & update it locally, in order to verify)
        sigma_protocol_verify(
            &sender_pk,
            &recipient_pk,
            &veiled_withdraw_amount,
            &veiled_deposit_amount,
            &transfer_proof.sigma_proof);

        // Verifies the range proofs in `transfer_proof` and withdraws `veiled_withdraw_amount` from the `sender`'s account.
        veiled_withdraw<CoinType>(
            veiled_withdraw_amount,
            sender_coin_store,
            &transfer_proof.new_balance_proof,
            &std::option::some(transfer_proof.veiled_amount_proof));

        // Creates a new veiled coin for the recipient.
        let vc = VeiledCoin<CoinType> { veiled_amount: veiled_deposit_amount };

        // Deposits `veiled_deposit_amount` into the recipient's account
        // (Note, if this aborts, the whole transaction aborts, so we do not need to worry about atomicity.)
        veiled_deposit(recipient_addr, vc);
    }

    /// Deposits a veiled `coin` at address `to_addr`.
    public fun veiled_deposit<CoinType>(to_addr: address, coin: VeiledCoin<CoinType>) acquires VeiledCoinStore {
        assert!(
            has_veiled_coin_store<CoinType>(to_addr),
            error::not_found(EVEILED_COIN_STORE_NOT_PUBLISHED),
        );

        let coin_store = borrow_global_mut<VeiledCoinStore<CoinType>>(to_addr);

        // Fetch the veiled balance
        let veiled_balance = elgamal::decompress_ciphertext(&coin_store.veiled_balance);

        // Subtract the veiled amount from it, homomorphically
        elgamal::ciphertext_add_assign(&mut veiled_balance, &coin.veiled_amount);

        // Update the veiled balance
        coin_store.veiled_balance = elgamal::compress_ciphertext(&veiled_balance);

        // Make sure the veiled coin is dropped so it cannot be double spent
        drop_veiled_coin(coin);

        // Once successful, emit an event that a veiled deposit occurred.
        event::emit_event<DepositEvent>(
            &mut coin_store.deposit_events,
            DepositEvent {},
        );
    }

    /// Withdraws a `veiled_amount` of coins from the specified coin store. Let `balance` denote its current
    /// *veiled* balance.
    ///
    /// **WARNING:** This function assumes that `veiled_amount` is correctly encrypted under the sender's PK. This
    /// is the case when either (1) the amount was veiled correctly from a public value or (2) a $\Sigma$-protocol proof
    /// over `veiled_amount` verified successfully.
    ///
    /// Always requires a ZK range proof `new_balance_proof` on `balance - amount`. When the veiled amount was NOT
    /// created from a public value, additionally requires a ZK range proof `veiled_amount_proof` on `amount`.
    public fun veiled_withdraw<CoinType>(
        veiled_amount: elgamal::Ciphertext,
        coin_store: &mut VeiledCoinStore<CoinType>,
        new_balance_proof: &RangeProof,
        veiled_amount_proof: &Option<RangeProof>)
    {
        // Fetch the ElGamal public key of the veiled account
        let pk = &coin_store.pk;

        // Fetch the veiled balance of the veiled account
        let veiled_balance = elgamal::decompress_ciphertext(&coin_store.veiled_balance);

        // Update the account's veiled balance by homomorphically subtracting the veiled amount from the veiled balance.
        elgamal::ciphertext_sub_assign(&mut veiled_balance, &veiled_amount);

        // This function checks if it is possible to withdraw a veiled `amount` from a veiled `bal`, obtaining a new
        // veiled balance `new_bal = bal - amount`. It maintains an invariant that `new_bal \in [0, 2^{32})` as follows.
        //
        //  1. We assume (by the invariant) that `bal \in [0, 2^{32})`.
        //
        //  2. We verify a ZK range proof that `amount \in [0, 2^{32})`. Otherwise, a sender could set `amount = p-1`
        //     where `p` is the order of the scalar field, which would give `new_bal = bal - (p-1) mod p = bal + 1`.
        //     Therefore, a malicious spender could create coins out of thin air for themselves.
        //
        //  3. We verify a ZK range proof that `new_bal \in [0, 2^{32})`. Otherwise, a sender could set `amount = bal + 1`,
        //     which would satisfy condition (2) from above but would give `new_bal = bal - (bal + 1) = -1`. Therefore,
        //     a malicious spender could spend more coins than they have.
        //
        // Altogether, these checks ensure that `bal - amount >= 0` (as integers) and therefore that `bal >= amount`
        // (again, as integers).
        //
        // When the caller of this function created the `veiled_amount` from a public `u32` value, the
        // `veiled_amount_proof` range proof is no longer necessary since the caller guarantees that condition (2) from
        // above holds.

        // Checks range condition (3)
        assert!(
            bulletproofs::verify_range_proof_elgamal(
                &veiled_balance,
                new_balance_proof,
                pk, MAX_BITS_IN_VALUE, VEILED_COIN_DST
            ),
            error::out_of_range(ERANGE_PROOF_VERIFICATION_FAILED)
        );

        // Checks range condition (2), if the veiled amount did not originate from a public amount
        if (std::option::is_some(veiled_amount_proof)) {
            assert!(
                bulletproofs::verify_range_proof_elgamal(
                    &veiled_amount,
                    std::option::borrow(veiled_amount_proof),
                    pk, MAX_BITS_IN_VALUE, VEILED_COIN_DST
                ),
                error::out_of_range(ERANGE_PROOF_VERIFICATION_FAILED)
            );
        };

        // Update the veiled balance to reflect the veiled withdrawal
        coin_store.veiled_balance = elgamal::compress_ciphertext(&veiled_balance);

        // Once everything succeeds, emit an event to indicate a veiled withdrawal occurred
        event::emit_event<WithdrawEvent>(
            &mut coin_store.withdraw_events,
            WithdrawEvent { },
        );
    }

    //
    // Private functions.
    //

    /// Given a vector `vec`, removes the last `cut_len` elements of `vec` and returns them in order. (This function
    /// exists because we did not like the interface of `std::vector::trim`.)
    fun cut_vector<T>(vec: &mut vector<T>, cut_len: u64): vector<T> {
        let len = vector::length(vec);
        let res = vector::empty();
        assert!(len >= cut_len, error::out_of_range(EVECTOR_CUT_TOO_LARGE));
        while (cut_len > 0) {
            vector::push_back(&mut res, vector::pop_back(vec));
            cut_len = cut_len - 1;
        };
        vector::reverse(&mut res);
        res
    }

    /// Returns a signer for the resource account storing all the normal coins that have been veiled.
    fun get_resource_account_signer(): signer acquires VeiledCoinMinter {
        account::create_signer_with_capability(&borrow_global<VeiledCoinMinter>(@veiled_coin).signer_cap)
    }

    /// Used internally to drop veiled coins that were split or joined.
    fun drop_veiled_coin<CoinType>(c: VeiledCoin<CoinType>) {
        let VeiledCoin<CoinType> { veiled_amount: _ } = c;
    }

    /// Deserializes and returns a `SigmaProof` given its byte representation (see protocol description in
    /// `sigma_protocol_verify`)
    ///
    /// Elements at the end of the `SigmaProof` struct are expected to be at the start  of the byte vector, and
    /// serialized using the serialization formats in the `ristretto255` module.
    fun deserialize_sigma_proof<CoinType>(proof_bytes: vector<u8>): Option<FullSigmaProof<CoinType>> {
        if (vector::length<u8>(&proof_bytes) != 160) {
            return std::option::none<FullSigmaProof<CoinType>>()
        };

        let x1_bytes = cut_vector<u8>(&mut proof_bytes, 32);
        let x1 = ristretto255::new_point_from_bytes(x1_bytes);
        if (!std::option::is_some<RistrettoPoint>(&x1)) {
            return std::option::none<FullSigmaProof<CoinType>>()
        };
        let x1 = std::option::extract<RistrettoPoint>(&mut x1);

        let x3_bytes = cut_vector<u8>(&mut proof_bytes, 32);
        let x3 = ristretto255::new_point_from_bytes(x3_bytes);
        if (!std::option::is_some<RistrettoPoint>(&x3)) {
            return std::option::none<FullSigmaProof<CoinType>>()
        };
        let x3 = std::option::extract<RistrettoPoint>(&mut x3);

        let x4_bytes = cut_vector<u8>(&mut proof_bytes, 32);
        let x4 = ristretto255::new_point_from_bytes(x4_bytes);
        if (!std::option::is_some<RistrettoPoint>(&x4)) {
            return std::option::none<FullSigmaProof<CoinType>>()
        };
        let x4 = std::option::extract<RistrettoPoint>(&mut x4);

        let alpha1_bytes = cut_vector<u8>(&mut proof_bytes, 32);
        let alpha1 = ristretto255::new_scalar_from_bytes(alpha1_bytes);
        if (!std::option::is_some(&alpha1)) {
            return std::option::none<FullSigmaProof<CoinType>>()
        };
        let alpha1 = std::option::extract(&mut alpha1);

        let alpha3_bytes = cut_vector<u8>(&mut proof_bytes, 32);
        let alpha3 = ristretto255::new_scalar_from_bytes(alpha3_bytes);
        if (!std::option::is_some(&alpha3)) {
            return std::option::none<FullSigmaProof<CoinType>>()
        };
        let alpha3 = std::option::extract(&mut alpha3);

        std::option::some(FullSigmaProof {
            x1, x3, x4, alpha1, alpha3
        })
    }

    /// Verifies a $\Sigma$-protocol proof necessary to ensure correctness of a veiled transfer.
    /// Specifically, this proof proves that `withdraw_ct` and `deposit_ct` encrypt the same amount $v$ using the same
    /// randomness $r$, with `sender_pk` and `recipient_pk` respectively.
    ///
    /// # Cryptographic details
    ///
    /// The proof argues knowledge of a witness $w$ such that a specific relation $R(x; w)$ is satisfied, for a public
    /// statement $x$ known to the verifier (i.e., known to the validators). We describe this relation below.
    ///
    /// The secret witness $w$ in this relation, known only to the sender of the TXN, consists of:
    ///  - $v$, the amount being transferred
    ///  - $r$, ElGamal encryption randomness
    ///
    /// (Note that the $\Sigma$-protocol's zero-knowledge property ensures the witness is not revealed.)
    ///
    /// The public statement $x$ in this relation consists of:
    ///  - $G$, the basepoint of a given elliptic curve
    ///  - $Y$, the sender's PK
    ///  - $Y'$, the recipient's PK
    ///  - $(C, D)$, the ElGamal encryption of $v$ under the sender's PK
    ///  - $(C', D)$, the ElGamal encryption of $v$ under the recipient's PK
    ///
    ///
    /// The relation, at a high level, and created two ciphertexts $(C, D)$ and $(C', D)$
    /// encrypting $v$ under the sender's PK and recipient's PK, respectively.:
    ///
    /// ```
    /// R(
    ///     x = [ Y, Y', (C, C', D), G]
    ///     w = [ v, r ]
    /// ) = {
    ///     C = v * G + r * Y
    ///     C' = v * G + r * Y'
    ///     D = r * G
    /// }
    /// ```
    ///
    /// A relation similar to this is also described on page 14 of the Zether paper [BAZB20] (just replace  $G$ -> $g$,
    /// $C'$ -> $\bar{C}$, $Y$ -> $y$, $Y'$ -> $\bar{y}$, $v$ -> $b^*$).
    ///
    /// Note the equations $C_L - C = b' G + sk (C_R - D)$ and $Y = sk G$ in the Zether paper are enforced
    /// programmatically by this smart contract and so are not needed in our $\Sigma$-protocol.
    fun sigma_protocol_verify<CoinType>(
        sender_pk: &elgamal::CompressedPubkey,
        recipient_pk: &elgamal::CompressedPubkey,
        withdraw_ct: &elgamal::Ciphertext,
        deposit_ct: &elgamal::Ciphertext,
        proof: &FullSigmaProof<CoinType>)
    {
        let sender_pk_point = elgamal::pubkey_to_point(sender_pk);
        let recipient_pk_point = elgamal::pubkey_to_point(recipient_pk);
        let (big_c, d) = elgamal::ciphertext_as_points(withdraw_ct);
        let (bar_c, _) = elgamal::ciphertext_as_points(deposit_ct);

        // TODO: Can be optimized so we don't re-serialize the proof for Fiat-Shamir
        let c = sigma_protocol_fiat_shamir<CoinType>(
            sender_pk, recipient_pk,
            withdraw_ct, deposit_ct,
            &proof.x1, &proof.x3, &proof.x4);

        // c * D + X1 =? \alpha_1 * g
        let d_acc = ristretto255::point_mul(d, &c);
        ristretto255::point_add_assign(&mut d_acc, &proof.x1);
        let g_alpha1 = ristretto255::basepoint_mul(&proof.alpha1);
        assert!(ristretto255::point_equals(&d_acc, &g_alpha1), error::invalid_argument(ESIGMA_PROTOCOL_VERIFY_FAILED));


        let g_alpha3 = ristretto255::basepoint_mul(&proof.alpha3);
        // c * C + X3 =? \alpha_3 * g + \alpha_1 * y
        let big_c_acc = ristretto255::point_mul(big_c, &c);
        ristretto255::point_add_assign(&mut big_c_acc, &proof.x3);
        let y_alpha1 = ristretto255::point_mul(&sender_pk_point, &proof.alpha1);
        ristretto255::point_add_assign(&mut y_alpha1, &g_alpha3);
        assert!(ristretto255::point_equals(&big_c_acc, &y_alpha1), error::invalid_argument(ESIGMA_PROTOCOL_VERIFY_FAILED));

        // c * \bar{C} + X4 =? \alpha_3 * g + \alpha_1 * \bar{y}
        let bar_c = ristretto255::point_mul(bar_c, &c);
        ristretto255::point_add_assign(&mut bar_c, &proof.x4);
        let bar_y_alpha1 = ristretto255::point_mul(&recipient_pk_point, &proof.alpha1);
        ristretto255::point_add_assign(&mut bar_y_alpha1, &g_alpha3);
        assert!(ristretto255::point_equals(&bar_c, &bar_y_alpha1), error::invalid_argument(ESIGMA_PROTOCOL_VERIFY_FAILED));
    }

    /// TODO: explain the challenge derivation as a function of the parameters
    /// Computes the challenge value as `c = H(g, y, \bar{y}, C, D, \bar{C}, X_1, X_3, X_4)`
    /// for the $\Sigma$-protocol from `verify_withdrawal_sigma_protocol` using the Fiat-Shamir transform. The notation
    /// used above is from the Zether [BAZB20] paper.
    fun sigma_protocol_fiat_shamir<CoinType>(
        sender_pk: &elgamal::CompressedPubkey,
        recipient_pk: &elgamal::CompressedPubkey,
        withdraw_ct: &elgamal::Ciphertext,
        deposit_ct: &elgamal::Ciphertext,
        x1: &RistrettoPoint,
        x3: &RistrettoPoint,
        x4: &RistrettoPoint): Scalar
    {
        let (big_c, d) = elgamal::ciphertext_as_points(withdraw_ct);
        let (bar_c, _) = elgamal::ciphertext_as_points(deposit_ct);

        // c <- H(g,y,\bar{y},C,D,\bar{C},X_1,X_3,X_4)
        let hash_input = vector::empty<u8>();

        let basepoint_bytes = ristretto255::point_to_bytes(&ristretto255::basepoint_compressed());
        vector::append<u8>(&mut hash_input, basepoint_bytes);

        let y = elgamal::pubkey_to_compressed_point(sender_pk);
        let y_bytes = ristretto255::point_to_bytes(&y);
        vector::append<u8>(&mut hash_input, y_bytes);

        let y_bar = elgamal::pubkey_to_compressed_point(recipient_pk);
        let y_bar_bytes = ristretto255::point_to_bytes(&y_bar);
        vector::append<u8>(&mut hash_input, y_bar_bytes);

        let big_c_bytes = ristretto255::point_to_bytes(&ristretto255::point_compress(big_c));
        vector::append<u8>(&mut hash_input, big_c_bytes);

        let d_bytes = ristretto255::point_to_bytes(&ristretto255::point_compress(d));
        vector::append<u8>(&mut hash_input, d_bytes);

        let bar_c_bytes = ristretto255::point_to_bytes(&ristretto255::point_compress(bar_c));
        vector::append<u8>(&mut hash_input, bar_c_bytes);

        let x_1_bytes = ristretto255::point_to_bytes(&ristretto255::point_compress(x1));
        vector::append<u8>(&mut hash_input, x_1_bytes);

        let x_3_bytes = ristretto255::point_to_bytes(&ristretto255::point_compress(x3));
        vector::append<u8>(&mut hash_input, x_3_bytes);

        let x_4_bytes = ristretto255::point_to_bytes(&ristretto255::point_compress(x4));
        vector::append<u8>(&mut hash_input, x_4_bytes);

        vector::append<u8>(&mut hash_input, FIAT_SHAMIR_SIGMA_DST);

        ristretto255::new_scalar_from_sha2_512(hash_input)
    }

    //
    // Test-only functions
    //

    #[test_only]
    /// Returns a random ElGamal keypair
    fun generate_elgamal_keypair(): (Scalar, elgamal::CompressedPubkey) {
        let sk = ristretto255::random_scalar();
        let pk = elgamal::pubkey_from_secret_key(&sk);
        (sk, pk)
    }

    #[test_only]
    /// Returns true if the balance at address `owner` equals `value`.
    /// Requires the ElGamal encryption randomness `r` and public key `pk` as auxiliary inputs.
    public fun verify_opened_balance<CoinType>(
        owner: address, value: u32, r: &Scalar, pk: &elgamal::CompressedPubkey): bool acquires VeiledCoinStore
    {
        // compute the expected encrypted balance
        let value = ristretto255::new_scalar_from_u32(value);
        let expected_ct = elgamal::new_ciphertext_with_basepoint(&value, r, pk);

        // get the actual encrypted balance
        let actual_ct = elgamal::decompress_ciphertext(&veiled_balance<CoinType>(owner));

        elgamal::ciphertext_equals(&actual_ct, &expected_ct)
    }

    #[test_only]
    /// Proves the $\Sigma$-protocol used for veiled coin transfers.
    /// See `sigma_protocol_verify` for a detailed description of the $\Sigma$-protocol
    public fun sigma_protocol_prove<CoinType>(
        sender_pk: &elgamal::CompressedPubkey,
        recipient_pk: &elgamal::CompressedPubkey,
        withdraw_ct: &elgamal::Ciphertext,
        deposit_ct: &elgamal::Ciphertext,
        amount_rand: &Scalar,
        amount_val: &Scalar): FullSigmaProof<CoinType>
   {
        let x1 = ristretto255::random_scalar();
        let x3 = ristretto255::random_scalar();

        // X1 <- g^{x1}
        let big_x1 = ristretto255::basepoint_mul(&x1);

        // X3 <- g^{x3}y^{x1}
        let big_x3 = ristretto255::basepoint_mul(&x3);
        let source_pk_point = elgamal::pubkey_to_point(sender_pk);
        let source_pk_x1 = ristretto255::point_mul(&source_pk_point, &x1);
        ristretto255::point_add_assign(&mut big_x3, &source_pk_x1);

        // X4 <- g^{x3}\bar{y}^{x1}
        let big_x4 = ristretto255::basepoint_mul(&x3);
        let dest_pk_point = elgamal::pubkey_to_point(recipient_pk);
        let dest_pk_x1 = ristretto255::point_mul(&dest_pk_point, &x1);
        ristretto255::point_add_assign(&mut big_x4, &dest_pk_x1);

        let c = sigma_protocol_fiat_shamir<CoinType>(
            sender_pk, recipient_pk,
            withdraw_ct,
            deposit_ct,
            &big_x1, &big_x3, &big_x4);

        // alpha_1 <- x1 + c * r
        let alpha1 = ristretto255::scalar_mul(&c, amount_rand);
        ristretto255::scalar_add_assign(&mut alpha1, &x1);

        // alpha_3 <- x3 + c * b^*
        let alpha3 = ristretto255::scalar_mul(&c, amount_val);
        ristretto255::scalar_add_assign(&mut alpha3, &x3);

        FullSigmaProof {
            x1: big_x1,
            x3: big_x3,
            x4: big_x4,
            alpha1,
            alpha3,
        }
    }

    #[test_only]
    /// Given a $\Sigma$-protocol proof, serializes it into byte form.
    /// Elements at the end of the `SigmaProof` struct are placed into the vector first,
    /// using the serialization formats in the `ristretto255` module.
    public fun serialize_sigma_proof<CoinType>(proof: &FullSigmaProof<CoinType>): vector<u8> {
        let x1_bytes = ristretto255::point_to_bytes(&ristretto255::point_compress(&proof.x1));
        let x3_bytes = ristretto255::point_to_bytes(&ristretto255::point_compress(&proof.x3));
        let x4_bytes = ristretto255::point_to_bytes(&ristretto255::point_compress(&proof.x4));
        let alpha1_bytes = ristretto255::scalar_to_bytes(&proof.alpha1);
        let alpha3_bytes = ristretto255::scalar_to_bytes(&proof.alpha3);

        let bytes = vector::empty<u8>();
        vector::append<u8>(&mut bytes, alpha3_bytes);
        vector::append<u8>(&mut bytes, alpha1_bytes);
        vector::append<u8>(&mut bytes, x4_bytes);
        vector::append<u8>(&mut bytes, x3_bytes);
        vector::append<u8>(&mut bytes, x1_bytes);

        bytes
    }

    //
    // Tests
    //

    #[test]
    fun sigma_proof_verify_test()
    {
        // Pick a keypair for the sender, and one for the recipient
        let (_, sender_pk) = generate_elgamal_keypair();
        let (_, recipient_pk) = generate_elgamal_keypair();

        // Set the transferred amount to 50
        let amount_val = ristretto255::new_scalar_from_u32(50);
        let amount_rand = ristretto255::random_scalar();
        // Encrypt the amount under the sender's PK
        let withdraw_ct= elgamal::new_ciphertext_with_basepoint(&amount_val, &amount_rand, &sender_pk);

        // Encrypt the amount under the recipient's PK
        let deposit_ct = elgamal::new_ciphertext_with_basepoint(&amount_val, &amount_rand, &recipient_pk);

        let sigma_proof = sigma_protocol_prove<coin::FakeMoney>(
            &sender_pk,
            &recipient_pk,
            &withdraw_ct,       // withdrawn amount, encrypted under sender PK
            &deposit_ct,        // deposited amount, encrypted under recipient PK (same plaintext as `withdraw_ct`)
            &amount_rand,       // encryption randomness for `withdraw_ct` and `deposit_ct`
            &amount_val,        // transferred amount
        );

        sigma_protocol_verify(&sender_pk, &recipient_pk, &withdraw_ct, &deposit_ct, &sigma_proof);
    }

    #[test]
    #[expected_failure(abort_code = 0x10008, location = Self)]
    fun sigma_proof_verify_fails_test()
    {
       let (_, source_pk) = generate_elgamal_keypair();
       let transfer_val = ristretto255::new_scalar_from_u32(50);
       let (_, dest_pk) = generate_elgamal_keypair();
       let transfer_rand = ristretto255::random_scalar();
       let (_, withdraw_ct) = bulletproofs::prove_range_elgamal(&transfer_val, &transfer_rand, &source_pk, MAX_BITS_IN_VALUE, VEILED_COIN_DST);
       let (_, deposit_ct) = bulletproofs::prove_range_elgamal(&transfer_val, &transfer_rand, &dest_pk, MAX_BITS_IN_VALUE, VEILED_COIN_DST);

       let sigma_proof = sigma_protocol_prove<coin::FakeMoney>(&source_pk, &dest_pk, &withdraw_ct, &deposit_ct, &transfer_rand, &transfer_val);

       let random_point = ristretto255::random_point();
       sigma_proof.x1 = random_point;

       sigma_protocol_verify(&source_pk, &dest_pk, &withdraw_ct, &deposit_ct, &sigma_proof);
    }

    #[test]
    fun sigma_proof_serialize_test()
    {
       let (_, source_pk) = generate_elgamal_keypair();
       let rand = ristretto255::random_scalar();
       let val = ristretto255::new_scalar_from_u32(50);
       let (_, dest_pk) = generate_elgamal_keypair();
       let source_randomness = ristretto255::scalar_neg(&rand);
       let (_, withdraw_ct) = bulletproofs::prove_range_elgamal(&val, &source_randomness, &source_pk, MAX_BITS_IN_VALUE, VEILED_COIN_DST);
       let (_, deposit_ct) = bulletproofs::prove_range_elgamal(&val, &rand, &dest_pk, MAX_BITS_IN_VALUE, VEILED_COIN_DST);

       let sigma_proof = sigma_protocol_prove<coin::FakeMoney>(&source_pk, &dest_pk, &withdraw_ct, &deposit_ct, &source_randomness, &val);

       let sigma_proof_bytes = serialize_sigma_proof<coin::FakeMoney>(&sigma_proof);

       let deserialized_proof = std::option::extract<FullSigmaProof<coin::FakeMoney>>(&mut deserialize_sigma_proof<coin::FakeMoney>(sigma_proof_bytes));

       assert!(ristretto255::point_equals(&sigma_proof.x1, &deserialized_proof.x1), 1);
       assert!(ristretto255::point_equals(&sigma_proof.x3, &deserialized_proof.x3), 1);
       assert!(ristretto255::point_equals(&sigma_proof.x4, &deserialized_proof.x4), 1);
       assert!(ristretto255::scalar_equals(&sigma_proof.alpha1, &deserialized_proof.alpha1), 1);
       assert!(ristretto255::scalar_equals(&sigma_proof.alpha3, &deserialized_proof.alpha3), 1);
    }

//    #[test(myself = @veiled_coin, source_fx = @aptos_framework, destination = @0x1337)]
//    fun wrap_to_test(
//        myself: signer,
//        source_fx: signer,
//        destination: signer
//    ) acquires VeiledCoinMinter, VeiledCoinStore {
//        // Initialize the `veiled_coin` module
//        init_module(&myself);
//
//        features::change_feature_flags(&source_fx, vector[features::get_bulletproofs_feature()], vector[]);
//
//        // Set up two accounts so we can register a new coin type on them
//        let source_addr = signer::address_of(&source_fx);
//        account::create_account_for_test(source_addr);
//        let destination_addr = signer::address_of(&destination);
//        account::create_account_for_test(destination_addr);
//
//        // Create some 1,000 fake money inside 'source'
//        coin::create_fake_money(&source_fx, &destination, 1000);
//
//        // Split 500 and 500 between source and destination
//        coin::transfer<coin::FakeMoney>(&source_fx, destination_addr, 500);
//
//        // Wrap 150 normal coins to veiled coins from the source's normal coin account
//        // to the destination's veiled coin account
//        let (_, destination_pk) = generate_elgamal_keypair();
//        let destination_pk_bytes = elgamal::pubkey_to_bytes(&destination_pk);
//        register<coin::FakeMoney>(&destination, destination_pk_bytes);
//        veil_to<coin::FakeMoney>(&source_fx, destination_addr, 150);
//        let source_balance = coin::balance<coin::FakeMoney>(source_addr);
//        assert!(source_balance == 350, 1);
//        let destination_rand = ristretto255::scalar_zero();
//        assert!(verify_opened_balance<coin::FakeMoney>(destination_addr, 150, &destination_rand, &destination_pk), 1);
//
//        // Unwrap back 50 veiled coins from the destination's veiled coin account to
//        // the source's normal coin account
//        let (_, source_pk) = generate_elgamal_keypair();
//        let source_pk_bytes = elgamal::pubkey_to_bytes(&source_pk);
//        register<coin::FakeMoney>(&source_fx, source_pk_bytes);
//
//        let destination_new_balance = ristretto255::new_scalar_from_u32(100);
//
//        let (new_balance_range_proof, _) = bulletproofs::prove_range_elgamal(&destination_new_balance, &destination_rand, &destination_pk, MAX_BITS_IN_VALUE, VEILED_COIN_DST);
//        let new_balance_range_proof_bytes = bulletproofs::range_proof_to_bytes(&new_balance_range_proof);
//
//        unveil_to<coin::FakeMoney>(&destination, source_addr, 50, new_balance_range_proof_bytes);
//        let source_balance = coin::balance<coin::FakeMoney>(source_addr);
//        assert!(source_balance == 400, 1);
//        assert!(verify_opened_balance<coin::FakeMoney>(destination_addr, 100, &destination_rand, &destination_pk), 1);
//    }

    #[test_only]
    fun println(str: vector<u8>) {
        print(&utf8(str));
    }

    #[test(myself = @veiled_coin, aptos_fx = @aptos_framework, sender = @0x1337)]
    fun unveil_test(
        myself: signer,
        aptos_fx: signer,
        sender: signer,
    ) acquires VeiledCoinMinter, VeiledCoinStore {
        println(b"Starting unveil_test()...");
        print(&@veiled_coin);
        print(&@aptos_framework);
        // TODO: This line seems to yield a strange, irreproducible invariant violation error...
        //assert!(@veiled_coin != @aptos_framework, 1);

        // Initialize the `veiled_coin` module & enable the feature
        init_module(&myself);
        println(b"Initialized module.");
        features::change_feature_flags(&aptos_fx, vector[features::get_bulletproofs_feature()], vector[]);
        println(b"Enabled feature flags.");

        // Set up the Aptos framework and `sender` accounts & register a new `FakeCoin` type on them
        account::create_account_for_test(signer::address_of(&aptos_fx)); // needed in `create_fake_money`
        account::create_account_for_test(signer::address_of(&sender));
        println(b"Created accounts for test.");

        // Create 500 coins in `aptos_fx` (must be Aptos 0x1 address) and registers a coin store for the `sender`
        coin::create_fake_money(&aptos_fx, &sender, cast_u32_to_u64_amount(500));
        // Moves the 500 coins into `sender` (because `create_fake_money` is awkward and doesn't actually do this)
        coin::transfer<coin::FakeMoney>(
            &aptos_fx, signer::address_of(&sender), cast_u32_to_u64_amount(500));

        // Register a veiled balance for the `sender`
        let (_, sender_pk) = generate_elgamal_keypair();
        register<coin::FakeMoney>(&sender, elgamal::pubkey_to_bytes(&sender_pk));
        println(b"Registered the sender's veiled balance");

        // Veil 150 out of the `sender`'s 500 coins.
        //
        // Note: Sender initializes his veiled balance to 150 veiled coins, which is why we don't need its SK to decrypt
        // it in order to transact.
        veil<coin::FakeMoney>(&sender, 150);
        println(b"Veiled 150 coins to the `sender`");

        println(b"Total veiled coins:");
        print(&total_veiled_coins<coin::FakeMoney>());

        assert!(total_veiled_coins<coin::FakeMoney>() == cast_u32_to_u64_amount(150), 1);

        // The `unveil` function uses randomness zero for the ElGamal encryption of the amount
        let sender_new_balance = ristretto255::new_scalar_from_u32(100);
        let zero_randomness = ristretto255::scalar_zero();

        // TODO: Will need a different wrapper function that creates the `UnveiledWithdrawalProof`
        let (new_balance_range_proof, _) = bulletproofs::prove_range_elgamal(
            &sender_new_balance,
            &zero_randomness,
            &sender_pk,
            MAX_BITS_IN_VALUE, VEILED_COIN_DST);

        // Move 50 veiled coins into the public balance of the sender
        unveil<coin::FakeMoney>(
            &sender,
            50,
            bulletproofs::range_proof_to_bytes(&new_balance_range_proof));

        println(b"Remaining veiled coins, after `unveil` call:");
        print(&total_veiled_coins<coin::FakeMoney>());

        assert!(total_veiled_coins<coin::FakeMoney>() == cast_u32_to_u64_amount(100), 1);

        assert!(verify_opened_balance<coin::FakeMoney>(
            signer::address_of(&sender), 100, &zero_randomness, &sender_pk), 2);

        let remaining_public_balance = coin::balance<coin::FakeMoney>(signer::address_of(&sender));
        assert!(remaining_public_balance == cast_u32_to_u64_amount(400), 3);
    }

    // TODO: test payments to self return

//    #[test(myself = @veiled_coin, source_fx = @aptos_framework, destination = @0x1337)]
//    fun basic_viability_test(
//        myself: signer,
//        source_fx: signer,
//        destination: signer
//    ) acquires VeiledCoinMinter, VeiledCoinStore {
//        // Initialize the `veiled_coin` module
//        init_module(&myself);
//
//        features::change_feature_flags(&source_fx, vector[features::get_bulletproofs_feature()], vector[]);
//
//        // Set up two accounts so we can register a new coin type on them
//        let source_addr = signer::address_of(&source_fx);
//        account::create_account_for_test(source_addr);
//        let destination_addr = signer::address_of(&destination);
//        account::create_account_for_test(destination_addr);
//
//        // Create some 1,000 fake money inside 'source'
//        coin::create_fake_money(&source_fx, &destination, 1000);
//
//        // Split 500 and 500 between source and destination
//        coin::transfer<coin::FakeMoney>(&source_fx, destination_addr, 500);
//
//        // Mint 150 veiled coins at source (requires registering a veiled coin store at 'source')
//        let (_, source_pk) = generate_elgamal_keypair();
//        let source_pk_bytes = elgamal::pubkey_to_bytes(&source_pk);
//        register<coin::FakeMoney>(&source_fx, source_pk_bytes);
//        veil<coin::FakeMoney>(&source_fx, 150);
//
//        // Transfer 50 of these veiled coins to destination
//        let transfer_val = ristretto255::new_scalar_from_u32(50);
//        let transfer_rand = ristretto255::random_scalar();
//
//        // This will be the balance left at the source, that we need to do a range proof for
//        let new_balance_rand_source = ristretto255::scalar_neg(&transfer_rand);
//        let source_new_balance = ristretto255::new_scalar_from_u32(100);
//        let (new_balance_range_proof, _) = bulletproofs::prove_range_elgamal(&source_new_balance, &new_balance_rand_source, &source_pk, MAX_BITS_IN_VALUE, VEILED_COIN_DST);
//
//        let (transferred_amount_range_proof, withdraw_ct) = bulletproofs::prove_range_elgamal(&transfer_val, &transfer_rand, &source_pk, MAX_BITS_IN_VALUE, VEILED_COIN_DST);
//
//        // Execute the veiled transaction: no one will be able to tell 50 coins are being transferred.
//        let (_, dest_pk) = generate_elgamal_keypair();
//        let dest_pk_bytes = elgamal::pubkey_to_bytes(&dest_pk);
//        register<coin::FakeMoney>(&destination, dest_pk_bytes);
//
//        let (_, deposit_ct) = bulletproofs::prove_range_elgamal(&transfer_val, &transfer_rand, &dest_pk, MAX_BITS_IN_VALUE, VEILED_COIN_DST);
//
//        let sigma_proof = sigma_protocol_prove<coin::FakeMoney>(&source_pk, &dest_pk, &withdraw_ct, &deposit_ct, &transfer_rand, &transfer_val);
//        let sigma_proof_bytes = serialize_sigma_proof<coin::FakeMoney>(&sigma_proof);
//        fully_veiled_transfer<coin::FakeMoney>(&source_fx, destination_addr, elgamal::ciphertext_to_bytes(&withdraw_ct), elgamal::ciphertext_to_bytes(&deposit_ct), bulletproofs::range_proof_to_bytes(&new_balance_range_proof), bulletproofs::range_proof_to_bytes(&transferred_amount_range_proof), sigma_proof_bytes);
//
//        // Unveil 25 coins from the source destination from veiled coins to regular coins
//        let source_new_balance_unveil = ristretto255::new_scalar_from_u32(75);
//
//        // Unveil doesn't change the randomness so we use the same randomness value as before
//        let (new_balance_range_proof_unveil, _) = bulletproofs::prove_range_elgamal(&source_new_balance_unveil, &new_balance_rand_source, &source_pk, MAX_BITS_IN_VALUE, VEILED_COIN_DST);
//        unveil<coin::FakeMoney>(&source_fx, 25, bulletproofs::range_proof_to_bytes(&new_balance_range_proof_unveil));
//
//        // Sanity check veiled balances
//        assert!(verify_opened_balance<coin::FakeMoney>(source_addr, 75, &new_balance_rand_source, &source_pk), 1);
//        assert!(verify_opened_balance<coin::FakeMoney>(destination_addr, 50, &transfer_rand, &dest_pk), 1);
//    }
}