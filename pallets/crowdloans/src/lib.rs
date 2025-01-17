// Copyright 2021 Parallel Finance Developer.
// This file is part of Parallel Finance.

// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// You may obtain a copy of the License at
// http://www.apache.org/licenses/LICENSE-2.0

// Unless required by applicable law or agreed to in writing, software
// distributed under the License is distributed on an "AS IS" BASIS,
// WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
// See the License for the specific language governing permissions and
// limitations under the License.

//! # Crowdloans pallet
//!
//! ## Overview
//!
//! Support your favorite parachains' crowdloans while releasing liquidity via crowdloans derivatives

#![cfg_attr(not(feature = "std"), no_std)]

mod benchmarking;

#[cfg(test)]
mod mock;
#[cfg(test)]
mod tests;

pub mod migrations;
pub mod types;
pub mod weights;
pub use pallet::*;
pub use weights::WeightInfo;

#[frame_support::pallet]
pub mod pallet {
    use super::*;
    use crate::types::*;
    use frame_support::{
        dispatch::DispatchResult,
        error::BadOrigin,
        log,
        pallet_prelude::*,
        require_transactional,
        storage::{child, ChildTriePrefixIterator},
        traits::{
            fungibles::{Inspect, Mutate, Transfer},
            Get, SortedMembers,
        },
        transactional, Blake2_128Concat, PalletId,
    };
    use frame_system::{
        ensure_signed,
        pallet_prelude::{BlockNumberFor, OriginFor},
    };
    use pallet_xcm::ensure_response;
    use primitives::{
        ArithmeticKind, Balance, CurrencyId, LeasePeriod, ParaId, Rate, TrieIndex, VaultId,
    };
    use sp_runtime::{
        traits::{
            AccountIdConversion, BlockNumberProvider, Hash, One, Saturating, StaticLookup, Zero,
        },
        ArithmeticError, DispatchError, FixedPointNumber, SaturatedConversion,
    };
    use sp_std::{boxed::Box, cmp::Ordering, vec::Vec};
    use xcm::latest::prelude::*;

    use pallet_traits::{
        DecimalProvider, Streaming, VaultTokenCurrenciesFilter, VaultTokenExchangeRateProvider,
    };

    use parallel_support::math_helper::f64::{
        fixed_u128_from_float, fixed_u128_to_float, power_float,
    };

    use pallet_xcm_helper::XcmHelper;

    pub type AccountIdOf<T> = <T as frame_system::Config>::AccountId;
    pub type AssetIdOf<T> =
        <<T as Config>::Assets as Inspect<<T as frame_system::Config>::AccountId>>::AssetId;
    pub type BalanceOf<T> =
        <<T as Config>::Assets as Inspect<<T as frame_system::Config>::AccountId>>::Balance;

    #[pallet::pallet]
    #[pallet::generate_store(pub(super) trait Store)]
    #[pallet::without_storage_info]
    pub struct Pallet<T>(_);

    macro_rules! ensure_origin {
        ($required_origin:ident, $origin:expr) => {
            if T::$required_origin::ensure_origin($origin.clone()).is_ok()
                || T::Members::contains(&ensure_signed($origin)?)
            {
                Ok(())
            } else {
                Err(DispatchError::from(BadOrigin))
            }
        };
    }

    #[pallet::config]
    pub trait Config: frame_system::Config + pallet_xcm::Config {
        type RuntimeEvent: From<Event<Self>> + IsType<<Self as frame_system::Config>::RuntimeEvent>;

        /// Assets for deposit/withdraw assets to/from crowdloan account
        type Assets: Transfer<Self::AccountId, AssetId = CurrencyId, Balance = Balance>
            + Inspect<Self::AccountId, AssetId = CurrencyId, Balance = Balance>
            + Mutate<Self::AccountId, AssetId = CurrencyId, Balance = Balance>;

        type RuntimeOrigin: IsType<<Self as frame_system::Config>::RuntimeOrigin>
            + Into<Result<pallet_xcm::Origin, <Self as Config>::RuntimeOrigin>>;

        type RuntimeCall: IsType<<Self as pallet_xcm::Config>::RuntimeCall> + From<Call<Self>>;

        /// Returns the parachain ID we are running with.
        #[pallet::constant]
        type SelfParaId: Get<ParaId>;

        /// Relay currency
        #[pallet::constant]
        type RelayCurrency: Get<AssetIdOf<Self>>;

        /// Pallet account for collecting contributions
        #[pallet::constant]
        type PalletId: Get<PalletId>;

        /// Minimum contribute amount
        #[pallet::constant]
        type MinContribution: Get<BalanceOf<Self>>;

        /// Maximum keys to be migrated in one extrinsic
        #[pallet::constant]
        type MigrateKeysLimit: Get<u32>;

        #[pallet::constant]
        type RemoveKeysLimit: Get<u32>; // default it to 1000

        /// LeasePeriod from relaychain
        #[pallet::constant]
        type LeasePeriod: Get<Self::BlockNumber>;

        /// LeaseOffset from relaychain
        #[pallet::constant]
        type LeaseOffset: Get<Self::BlockNumber>;

        /// LeaseOffset from relaychain
        #[pallet::constant]
        type LeasePerYear: Get<Self::BlockNumber>;

        /// The origin which can update global proxy address
        type ProxyOrigin: EnsureOrigin<<Self as frame_system::Config>::RuntimeOrigin>;

        /// The origin which can migrate pending contribution
        type MigrateOrigin: EnsureOrigin<<Self as frame_system::Config>::RuntimeOrigin>;

        /// The origin which can set vrf flag
        type VrfOrigin: EnsureOrigin<<Self as frame_system::Config>::RuntimeOrigin>;

        /// The origin which can create vault
        type CreateOrigin: EnsureOrigin<<Self as frame_system::Config>::RuntimeOrigin>;

        /// The origin which can refund
        type RefundOrigin: EnsureOrigin<<Self as frame_system::Config>::RuntimeOrigin>;

        /// The origin which can dissolve vault
        type DissolveOrigin: EnsureOrigin<<Self as frame_system::Config>::RuntimeOrigin>;

        /// The origin which can update vault
        type UpdateOrigin: EnsureOrigin<<Self as frame_system::Config>::RuntimeOrigin>;

        /// The origin which can close/reopen vault
        type OpenCloseOrigin: EnsureOrigin<<Self as frame_system::Config>::RuntimeOrigin>;

        /// The origin which can call auction failed/succeeded
        type AuctionSucceededFailedOrigin: EnsureOrigin<
            <Self as frame_system::Config>::RuntimeOrigin,
        >;

        /// The origin which can call slot expired
        type SlotExpiredOrigin: EnsureOrigin<<Self as frame_system::Config>::RuntimeOrigin>;

        /// Approved automation group for Phase Transition, Vrf, VaultCreation, VaultUpdate, Refund and Dissolve
        type Members: SortedMembers<Self::AccountId>;

        /// Weight information
        type WeightInfo: WeightInfo;

        /// The relay's BlockNumber provider
        type RelayChainBlockNumberProvider: BlockNumberProvider<BlockNumber = BlockNumberFor<Self>>;

        /// To expose XCM helper functions
        type XCM: XcmHelper<Self, BalanceOf<Self>, Self::AccountId>;

        /// To expose Streaming related functions
        type Streaming: Streaming<Self::AccountId, AssetIdOf<Self>, BalanceOf<Self>>;

        /// The asset id for native currency.
        #[pallet::constant]
        type GetNativeCurrencyId: Get<AssetIdOf<Self>>;

        /// Decimal provider.
        type Decimal: DecimalProvider<CurrencyId>;
    }

    #[pallet::event]
    #[pallet::generate_deposit(pub(super) fn deposit_event)]
    pub enum Event<T: Config> {
        /// New vault was created
        /// [para_id, vault_id, ctoken_id, phase, contribution_strategy, cap, end_block, trie_index]
        VaultCreated(
            ParaId,
            VaultId,
            AssetIdOf<T>,
            VaultPhase,
            ContributionStrategy,
            BalanceOf<T>,
            BlockNumberFor<T>,
            TrieIndex,
        ),
        /// Existing vault was updated
        /// [para_id, vault_id, contribution_strategy, cap, end_block]
        VaultUpdated(
            ParaId,
            VaultId,
            ContributionStrategy,
            BalanceOf<T>,
            BlockNumberFor<T>,
        ),
        /// Vault was opened
        /// [para_id, vault_id, pre_phase, now_phase]
        VaultPhaseUpdated(ParaId, VaultId, VaultPhase, VaultPhase),
        /// Vault is trying to do contributing
        /// [para_id, vault_id, contributor, amount, referral_code]
        VaultDoContributing(ParaId, VaultId, T::AccountId, BalanceOf<T>, Vec<u8>),
        /// Vault is trying to do withdrawing
        /// [para_id, vault_id, amount, target_phase]
        VaultDoWithdrawing(ParaId, VaultId, BalanceOf<T>, VaultPhase),
        /// Vault successfully contributed
        /// [para_id, vault_id, contributor, amount, referral_code]
        VaultContributed(ParaId, VaultId, T::AccountId, BalanceOf<T>, Vec<u8>),
        /// A user claimed CToken from vault
        /// [para_id, vault_id, ctoken_id, account, amount, phase]
        VaultClaimed(
            ParaId,
            VaultId,
            AssetIdOf<T>,
            T::AccountId,
            BalanceOf<T>,
            VaultPhase,
        ),
        /// A user withdrew contributed assets from vault
        /// [para_id, vault_id, account, amount, phase]
        VaultWithdrew(ParaId, VaultId, T::AccountId, BalanceOf<T>, VaultPhase),
        /// A user redeemed contributed assets using CToken
        /// [para_id, vault_id, ctoken_id, account, amount, phase]
        VaultRedeemed(
            ParaId,
            VaultId,
            AssetIdOf<T>,
            T::AccountId,
            BalanceOf<T>,
            VaultPhase,
        ),
        /// Vrfs updated
        /// [vrf_flag]
        VrfUpdated(bool),
        /// Notification received
        /// [multi_location, query_id, res]
        NotificationReceived(Box<MultiLocation>, QueryId, Option<(u32, XcmError)>),
        /// All contributions migrated
        /// [para_id, vault_id]
        AllMigrated(ParaId, VaultId),
        /// Partially contributions migrated
        /// [para_id, vault_id]
        PartiallyMigrated(ParaId, VaultId),
        /// Vault has been dissolved
        /// [para_id, vault_id]
        VaultDissolved(ParaId, VaultId),
        /// Partially Refunded
        /// [para_id, vault_id]
        AllRefunded(ParaId, VaultId),
        /// Partially Refunded
        /// [para_id, vault_id]
        PartiallyRefunded(ParaId, VaultId),
        /// Refunded
        /// [para_id, vault_id, account, child_storage_kind, amount]
        UserRefunded(
            ParaId,
            VaultId,
            T::AccountId,
            ChildStorageKind,
            BalanceOf<T>,
        ),
        /// Update proxy address
        /// [account]
        ProxyUpdated(T::AccountId),
        /// Update leases bonus
        LeasesBonusUpdated(VaultId, BonusConfig<BalanceOf<T>>),
    }

    #[pallet::error]
    pub enum Error<T> {
        /// Vault is not in correct phase
        IncorrectVaultPhase,
        /// Crowdloan ParaId already exists
        CrowdloanAlreadyExists,
        /// Contribution is not enough
        InsufficientContribution,
        /// There are no contributions stored in contributed childstorage
        NoContributions,
        /// Balance is not enough
        InsufficientBalance,
        /// Last lease period must be greater than first lease period.
        LastPeriodBeforeFirstPeriod,
        /// CToken does not exist
        CTokenDoesNotExist,
        /// Vault already exists
        VaultAlreadyExists,
        /// Vault does not exist
        VaultDoesNotExist,
        /// CToken for provided (leaseStart, leaseEnd) is different with what has been created previously
        InvalidCToken,
        /// Vault for provided ParaId not ended
        VaultNotEnded,
        /// No contributions allowed during the VRF delay
        VrfDelayInProgress,
        /// Attempted contribution violates contribution cap
        CapExceeded,
        /// Current relay block is greater than vault end block
        EndBlockExceeded,
        /// Capacity cannot be zero value
        InvalidCap,
        /// Invalid params input
        InvalidParams,
        /// Vault is not ready to be dissolved
        NotReadyToDissolve,
        /// Proxy address is empty
        EmptyProxyAddress,
        /// BonusConfig is wrong
        WrongBonusConfig,
    }

    #[pallet::storage]
    #[pallet::getter(fn vaults)]
    pub type Vaults<T: Config> = StorageNMap<
        _,
        (
            NMapKey<Blake2_128Concat, ParaId>,
            NMapKey<Blake2_128Concat, LeasePeriod>,
            NMapKey<Blake2_128Concat, LeasePeriod>,
        ),
        Vault<T>,
        OptionQuery,
    >;

    #[pallet::storage]
    #[pallet::getter(fn is_vrf)]
    pub type IsVrf<T: Config> = StorageValue<_, bool, ValueQuery>;

    #[pallet::storage]
    #[pallet::getter(fn ctoken_of)]
    pub type CTokensRegistry<T: Config> = StorageNMap<
        _,
        (
            NMapKey<Blake2_128Concat, LeasePeriod>,
            NMapKey<Blake2_128Concat, LeasePeriod>,
        ),
        AssetIdOf<T>,
        OptionQuery,
    >;

    #[pallet::storage]
    #[pallet::getter(fn current_lease)]
    pub type LeasesRegistry<T: Config> =
        StorageMap<_, Blake2_128Concat, ParaId, (LeasePeriod, LeasePeriod), OptionQuery>;

    #[pallet::storage]
    #[pallet::getter(fn next_trie_index)]
    pub type NextTrieIndex<T> = StorageValue<_, TrieIndex, ValueQuery>;

    #[pallet::storage]
    #[pallet::getter(fn xcm_request)]
    pub type XcmRequests<T> = StorageMap<_, Blake2_128Concat, QueryId, XcmRequest<T>, OptionQuery>;

    /// Storage version of the pallet.
    #[pallet::storage]
    pub type StorageVersion<T: Config> = StorageValue<_, Releases, ValueQuery>;

    #[pallet::storage]
    #[pallet::getter(fn proxy_address)]
    pub type ProxyAddress<T: Config> = StorageValue<_, AccountIdOf<T>, OptionQuery>;

    #[pallet::storage]
    #[pallet::getter(fn leases_bonus)]
    pub type LeasesBonus<T: Config> = StorageNMap<
        _,
        (
            NMapKey<Blake2_128Concat, LeasePeriod>,
            NMapKey<Blake2_128Concat, LeasePeriod>,
        ),
        BonusConfig<BalanceOf<T>>,
        ValueQuery,
    >;

    #[pallet::call]
    impl<T: Config> Pallet<T> {
        /// Create a new vault via a governance decision
        ///
        /// - `crowdloan`: parachain id of the crowdloan, should be consistent with relaychain
        /// - `ctoken`: ctoken is used for the vault, should be unique
        /// - `lease_start`: lease start index
        /// - `lease_end`: lease end index
        /// - `contribution_strategy`: currently, only XCM strategy is supported.
        /// - `cap`: the capacity limit for the vault
        /// - `end_block`: the crowdloan end block for the vault
        #[pallet::call_index(0)]
        #[pallet::weight(<T as Config>::WeightInfo::create_vault())]
        #[transactional]
        pub fn create_vault(
            origin: OriginFor<T>,
            crowdloan: ParaId,
            ctoken: AssetIdOf<T>,
            lease_start: LeasePeriod,
            lease_end: LeasePeriod,
            contribution_strategy: ContributionStrategy,
            #[pallet::compact] cap: BalanceOf<T>,
            end_block: BlockNumberFor<T>,
        ) -> DispatchResult {
            ensure_origin!(CreateOrigin, origin)?;

            ensure!(!cap.is_zero(), Error::<T>::InvalidCap);

            ensure!(
                lease_start <= lease_end,
                Error::<T>::LastPeriodBeforeFirstPeriod
            );

            if let Some(c) = Self::ctoken_of((&lease_start, &lease_end)) {
                ensure!(c == ctoken, Error::<T>::InvalidCToken);
            }

            ensure!(
                !Vaults::<T>::contains_key((&crowdloan, &lease_start, &lease_end)),
                Error::<T>::VaultAlreadyExists
            );

            // origin shouldn't be able to create a new vault if the previous one is not finished
            if let Some(vault) = Self::current_vault(crowdloan) {
                if vault.phase != VaultPhase::Failed && vault.phase != VaultPhase::Expired {
                    return Err(DispatchError::from(Error::<T>::VaultNotEnded));
                }
            }

            ensure!(
                T::RelayChainBlockNumberProvider::current_block_number() <= end_block,
                Error::<T>::EndBlockExceeded
            );

            let trie_index = Self::next_trie_index();
            let next_trie_index = trie_index.checked_add(1).ok_or(ArithmeticError::Overflow)?;
            let new_vault = Vault::new(
                lease_start,
                lease_end,
                ctoken,
                contribution_strategy,
                cap,
                end_block,
                trie_index,
            );

            log::trace!(
                target: "crowdloans::create_vault",
                "para_id: {:?}, lease_start: {:?}, lease_end: {:?}, trie_index: {:?}, ctoken: {:?}",
                crowdloan,
                lease_start,
                lease_end,
                trie_index,
                ctoken
            );

            NextTrieIndex::<T>::put(next_trie_index);
            Vaults::<T>::insert((&crowdloan, &lease_start, &lease_end), new_vault);
            CTokensRegistry::<T>::insert((&lease_start, &lease_end), ctoken);
            LeasesRegistry::<T>::insert(crowdloan, (lease_start, lease_end));

            Self::deposit_event(Event::<T>::VaultCreated(
                crowdloan,
                (lease_start, lease_end),
                ctoken,
                VaultPhase::Pending,
                contribution_strategy,
                cap,
                end_block,
                trie_index,
            ));

            Ok(())
        }

        /// Update an existing vault via a governance decision
        #[pallet::call_index(1)]
        #[pallet::weight(<T as Config>::WeightInfo::update_vault())]
        #[transactional]
        pub fn update_vault(
            origin: OriginFor<T>,
            crowdloan: ParaId,
            cap: Option<BalanceOf<T>>,
            end_block: Option<BlockNumberFor<T>>,
            contribution_strategy: Option<ContributionStrategy>,
        ) -> DispatchResult {
            ensure_origin!(UpdateOrigin, origin)?;

            let mut vault = Self::current_vault(crowdloan).ok_or(Error::<T>::VaultDoesNotExist)?;

            if let Some(cap) = cap {
                ensure!(!cap.is_zero(), Error::<T>::InvalidCap);
                vault.cap = cap;
            }

            if let Some(end_block) = end_block {
                ensure!(
                    T::RelayChainBlockNumberProvider::current_block_number() <= end_block,
                    Error::<T>::EndBlockExceeded
                );
                vault.end_block = end_block;
            }

            if let Some(contribution_strategy) = contribution_strategy {
                vault.contribution_strategy = contribution_strategy;
            }

            let Vault {
                lease_start,
                lease_end,
                contribution_strategy,
                cap,
                end_block,
                ..
            } = vault;

            Vaults::<T>::insert((&crowdloan, &lease_start, &lease_end), vault);

            Self::deposit_event(Event::<T>::VaultUpdated(
                crowdloan,
                (lease_start, lease_end),
                contribution_strategy,
                cap,
                end_block,
            ));

            Ok(())
        }

        /// Mark the associated vault as ready for real contributions on the relaychain
        #[pallet::call_index(2)]
        #[pallet::weight(<T as Config>::WeightInfo::open())]
        #[transactional]
        pub fn open(origin: OriginFor<T>, crowdloan: ParaId) -> DispatchResult {
            ensure_origin!(OpenCloseOrigin, origin)?;

            log::trace!(
                target: "crowdloans::open",
                "pre-toggle. crowdloan: {:?}",
                crowdloan,
            );

            Self::try_mutate_vault(crowdloan, VaultPhase::Pending, |vault| {
                vault.phase = VaultPhase::Contributing;
                Self::deposit_event(Event::<T>::VaultPhaseUpdated(
                    crowdloan,
                    (vault.lease_start, vault.lease_end),
                    VaultPhase::Pending,
                    VaultPhase::Contributing,
                ));
                Ok(())
            })
        }

        /// Contribute `amount` to the vault of `crowdloan` and receive some
        /// shares from it
        #[pallet::call_index(3)]
        #[pallet::weight(<T as Config>::WeightInfo::contribute())]
        #[transactional]
        pub fn contribute(
            origin: OriginFor<T>,
            crowdloan: ParaId,
            #[pallet::compact] amount: BalanceOf<T>,
            referral_code: Vec<u8>,
        ) -> DispatchResultWithPostInfo {
            let who = ensure_signed(origin)?;

            let mut vault = Self::current_vault(crowdloan).ok_or(Error::<T>::VaultDoesNotExist)?;

            ensure!(!amount.is_zero(), Error::<T>::InvalidParams);

            ensure!(
                T::RelayChainBlockNumberProvider::current_block_number() <= vault.end_block,
                Error::<T>::EndBlockExceeded
            );

            ensure!(
                vault.phase == VaultPhase::Contributing || vault.phase == VaultPhase::Pending,
                Error::<T>::IncorrectVaultPhase
            );

            ensure!(
                amount >= T::MinContribution::get(),
                Error::<T>::InsufficientContribution
            );

            ensure!(!Self::is_vrf(), Error::<T>::VrfDelayInProgress);

            ensure!(
                Self::total_contribution(&vault)?
                    .checked_add(amount)
                    .ok_or(ArithmeticError::Overflow)?
                    <= vault.cap,
                Error::<T>::CapExceeded
            );

            T::Assets::transfer(
                T::RelayCurrency::get(),
                &who,
                &Self::account_id(),
                amount,
                false,
            )?;

            if vault.phase == VaultPhase::Contributing {
                Self::do_update_contribution(
                    &who,
                    &mut vault,
                    amount,
                    Some(referral_code.clone()),
                    ArithmeticKind::Addition,
                    ChildStorageKind::Flying,
                )?;

                Self::do_contribute(
                    &who,
                    crowdloan,
                    (vault.lease_start, vault.lease_end),
                    vault.contribution_strategy,
                    amount,
                    referral_code.clone(),
                )?;
            } else {
                Self::do_update_contribution(
                    &who,
                    &mut vault,
                    amount,
                    Some(referral_code.clone()),
                    ArithmeticKind::Addition,
                    ChildStorageKind::Pending,
                )?;
            }

            Vaults::<T>::insert(
                (
                    &crowdloan,
                    &vault.lease_start.clone(),
                    &vault.lease_end.clone(),
                ),
                vault,
            );

            log::trace!(
                target: "crowdloans::contribute",
                "who: {:?}, para_id: {:?}, amount: {:?}, referral_code: {:?}",
                &who,
                &crowdloan,
                &amount,
                &referral_code
            );

            Ok(().into())
        }

        /// Set crowdloans which entered vrf period
        #[pallet::call_index(4)]
        #[pallet::weight(<T as Config>::WeightInfo::set_vrf())]
        #[transactional]
        pub fn set_vrf(origin: OriginFor<T>, flag: bool) -> DispatchResult {
            ensure_origin!(VrfOrigin, origin)?;

            log::trace!(
                target: "crowdloans::set_vrf",
                "pre-toggle. flag: {:?}",
                flag
            );
            IsVrf::<T>::put(flag);

            Self::deposit_event(Event::<T>::VrfUpdated(flag));

            Ok(())
        }

        /// Mark the associated vault as `Closed` and stop accepting contributions
        #[pallet::call_index(5)]
        #[pallet::weight(<T as Config>::WeightInfo::close())]
        #[transactional]
        pub fn close(origin: OriginFor<T>, crowdloan: ParaId) -> DispatchResult {
            ensure_origin!(OpenCloseOrigin, origin)?;

            log::trace!(
                target: "crowdloans::close",
                "pre-toggle. crowdloan: {:?}",
                crowdloan,
            );

            Self::try_mutate_vault(crowdloan, VaultPhase::Contributing, |vault| {
                vault.phase = VaultPhase::Closed;
                Self::deposit_event(Event::<T>::VaultPhaseUpdated(
                    crowdloan,
                    (vault.lease_start, vault.lease_end),
                    VaultPhase::Contributing,
                    VaultPhase::Closed,
                ));
                Ok(())
            })
        }

        /// Mark the associated vault as `Contributing` and continue to accept contributions
        #[pallet::call_index(6)]
        #[pallet::weight(<T as Config>::WeightInfo::reopen())]
        #[transactional]
        pub fn reopen(origin: OriginFor<T>, crowdloan: ParaId) -> DispatchResult {
            ensure_origin!(OpenCloseOrigin, origin)?;

            log::trace!(
                target: "crowdloans::reopen",
                "pre-toggle. crowdloan: {:?}",
                crowdloan,
            );

            Self::try_mutate_vault(crowdloan, VaultPhase::Closed, |vault| {
                vault.phase = VaultPhase::Contributing;
                Self::deposit_event(Event::<T>::VaultPhaseUpdated(
                    crowdloan,
                    (vault.lease_start, vault.lease_end),
                    VaultPhase::Closed,
                    VaultPhase::Contributing,
                ));
                Ok(())
            })
        }

        /// Mark the associated vault as `Succeed` if vault is `Closed`
        #[pallet::call_index(7)]
        #[pallet::weight(<T as Config>::WeightInfo::auction_succeeded())]
        #[transactional]
        pub fn auction_succeeded(origin: OriginFor<T>, crowdloan: ParaId) -> DispatchResult {
            ensure_origin!(AuctionSucceededFailedOrigin, origin)?;

            log::trace!(
                target: "crowdloans::auction_succeeded",
                "pre-toggle. crowdloan: {:?}",
                crowdloan,
            );

            Self::try_mutate_vault(crowdloan, VaultPhase::Closed, |vault| {
                vault.phase = VaultPhase::Succeeded;
                Self::deposit_event(Event::<T>::VaultPhaseUpdated(
                    crowdloan,
                    (vault.lease_start, vault.lease_end),
                    VaultPhase::Closed,
                    VaultPhase::Succeeded,
                ));
                Ok(())
            })
        }

        /// If a `crowdloan` failed, get the coins back and mark the vault as ready
        /// for distribution
        #[pallet::call_index(8)]
        #[pallet::weight(<T as Config>::WeightInfo::auction_failed())]
        #[transactional]
        pub fn auction_failed(origin: OriginFor<T>, crowdloan: ParaId) -> DispatchResult {
            ensure_origin!(AuctionSucceededFailedOrigin, origin)?;

            log::trace!(
                target: "crowdloans::auction_failed",
                "pre-toggle. crowdloan: {:?}",
                crowdloan,
            );

            Self::try_mutate_vault(crowdloan, VaultPhase::Closed, |vault| {
                Self::do_withdraw(
                    crowdloan,
                    (vault.lease_start, vault.lease_end),
                    vault.contributed,
                    VaultPhase::Failed,
                )?;
                Ok(())
            })
        }

        /// If a `crowdloan` succeeded, claim the liquid derivatives of the
        /// contributed assets
        #[pallet::call_index(9)]
        #[pallet::weight(<T as Config>::WeightInfo::claim())]
        #[transactional]
        pub fn claim(
            origin: OriginFor<T>,
            crowdloan: ParaId,
            lease_start: LeasePeriod,
            lease_end: LeasePeriod,
        ) -> DispatchResult {
            let who = ensure_signed(origin)?;
            Self::do_claim_for(who, crowdloan, lease_start, lease_end)
        }

        /// If a `crowdloan` succeeded, claim the liquid derivatives of the
        /// contributed assets for others
        #[pallet::call_index(10)]
        #[pallet::weight(<T as Config>::WeightInfo::claim())]
        #[transactional]
        pub fn claim_for(
            origin: OriginFor<T>,
            dest: <T::Lookup as StaticLookup>::Source,
            crowdloan: ParaId,
            lease_start: LeasePeriod,
            lease_end: LeasePeriod,
        ) -> DispatchResult {
            let _ = ensure_signed(origin)?;
            let who = T::Lookup::lookup(dest)?;
            Self::do_claim_for(who, crowdloan, lease_start, lease_end)
        }

        /// If a `crowdloan` failed, withdraw the contributed assets
        #[pallet::call_index(11)]
        #[pallet::weight(<T as Config>::WeightInfo::withdraw())]
        #[transactional]
        pub fn withdraw(
            origin: OriginFor<T>,
            crowdloan: ParaId,
            lease_start: LeasePeriod,
            lease_end: LeasePeriod,
        ) -> DispatchResult {
            let who = ensure_signed(origin)?;
            Self::do_withdraw_for(who, crowdloan, lease_start, lease_end)
        }

        /// If a `crowdloan` failed, withdraw the contributed assets for others
        #[pallet::call_index(12)]
        #[pallet::weight(<T as Config>::WeightInfo::withdraw())]
        #[transactional]
        pub fn withdraw_for(
            origin: OriginFor<T>,
            dest: <T::Lookup as StaticLookup>::Source,
            crowdloan: ParaId,
            lease_start: LeasePeriod,
            lease_end: LeasePeriod,
        ) -> DispatchResult {
            let _ = ensure_signed(origin)?;
            let who = T::Lookup::lookup(dest)?;
            Self::do_withdraw_for(who, crowdloan, lease_start, lease_end)
        }

        /// If a `crowdloan` expired, redeem the contributed assets
        /// using ctoken
        #[pallet::call_index(13)]
        #[pallet::weight(<T as Config>::WeightInfo::redeem())]
        #[transactional]
        pub fn redeem(
            origin: OriginFor<T>,
            crowdloan: ParaId,
            lease_start: LeasePeriod,
            lease_end: LeasePeriod,
            #[pallet::compact] amount: BalanceOf<T>,
        ) -> DispatchResult {
            let who = ensure_signed(origin)?;

            let ctoken = Self::ctoken_of((&lease_start, &lease_end))
                .ok_or(Error::<T>::CTokenDoesNotExist)?;
            let mut vault = Self::vaults((&crowdloan, &lease_start, &lease_end))
                .ok_or(Error::<T>::VaultDoesNotExist)?;

            ensure!(
                vault.phase == VaultPhase::Expired,
                Error::<T>::IncorrectVaultPhase
            );

            log::trace!(
                target: "crowdloans::redeem",
                "who: {:?}, ctoken: {:?}, amount: {:?}, para_id: {:?}, lease_start: {:?}, lease_end: {:?}",
                &who,
                &ctoken,
                &amount,
                &crowdloan,
                &lease_start,
                &lease_end
            );

            let ctoken_balance = T::Assets::reducible_balance(ctoken, &who, false);
            ensure!(ctoken_balance >= amount, Error::<T>::InsufficientBalance);

            vault.contributed = vault
                .contributed
                .checked_sub(amount)
                .ok_or(ArithmeticError::Underflow)?;

            T::Assets::burn_from(ctoken, &who, amount)?;
            // SovereignAccount on relaychain must have
            // withdrawn the contribution
            T::Assets::mint_into(T::RelayCurrency::get(), &who, amount)?;

            Vaults::<T>::insert((&crowdloan, &lease_start, &lease_end), vault);

            Self::deposit_event(Event::<T>::VaultRedeemed(
                crowdloan,
                (lease_start, lease_end),
                ctoken,
                who,
                amount,
                VaultPhase::Expired,
            ));

            Ok(())
        }

        /// If a `crowdloan` succeeded and its slot expired, use `call` to
        /// claim back the funds lent to the parachain
        #[pallet::call_index(14)]
        #[pallet::weight(<T as Config>::WeightInfo::slot_expired())]
        #[transactional]
        pub fn slot_expired(origin: OriginFor<T>, crowdloan: ParaId) -> DispatchResult {
            ensure_origin!(SlotExpiredOrigin, origin)?;

            log::trace!(
                target: "crowdloans::slot_expired",
                "pre-toggle. crowdloan: {:?}",
                crowdloan,
            );

            Self::try_mutate_vault(crowdloan, VaultPhase::Succeeded, |vault| {
                Self::do_withdraw(
                    crowdloan,
                    (vault.lease_start, vault.lease_end),
                    vault.contributed,
                    VaultPhase::Expired,
                )?;
                Ok(())
            })
        }

        /// Migrate pending contribution by sending xcm
        #[pallet::call_index(15)]
        #[pallet::weight(<T as Config>::WeightInfo::migrate_pending())]
        #[transactional]
        pub fn migrate_pending(origin: OriginFor<T>, crowdloan: ParaId) -> DispatchResult {
            ensure_origin!(MigrateOrigin, origin)?;

            let mut vault = Self::current_vault(crowdloan).ok_or(Error::<T>::VaultDoesNotExist)?;
            ensure!(
                vault.phase == VaultPhase::Pending || vault.phase == VaultPhase::Contributing,
                Error::<T>::IncorrectVaultPhase
            );
            ensure!(!Self::is_vrf(), Error::<T>::VrfDelayInProgress);

            let contributions =
                Self::contribution_iterator(vault.trie_index, ChildStorageKind::Pending);
            let mut migrated_count = 0u32;
            let mut all_migrated = true;

            // single migration has a processing limit
            for (who, (amount, referral_code)) in contributions {
                if migrated_count >= T::MigrateKeysLimit::get() {
                    all_migrated = false;
                    break;
                }
                Self::do_migrate_contribution(
                    &who,
                    &mut vault,
                    amount,
                    ChildStorageKind::Pending,
                    ChildStorageKind::Flying,
                )?;
                Self::do_contribute(
                    &who,
                    crowdloan,
                    (vault.lease_start, vault.lease_end),
                    vault.contribution_strategy,
                    amount,
                    referral_code,
                )?;
                migrated_count += 1;
            }

            let Vault {
                lease_start,
                lease_end,
                ..
            } = vault;

            Vaults::<T>::insert((&crowdloan, &lease_start, &lease_end), vault);

            if all_migrated {
                Self::deposit_event(Event::<T>::AllMigrated(crowdloan, (lease_start, lease_end)));
            } else {
                Self::deposit_event(Event::<T>::PartiallyMigrated(
                    crowdloan,
                    (lease_start, lease_end),
                ));
            }

            Ok(())
        }

        #[pallet::call_index(16)]
        #[pallet::weight(<T as Config>::WeightInfo::notification_received())]
        #[transactional]
        pub fn notification_received(
            origin: OriginFor<T>,
            query_id: QueryId,
            response: Response,
        ) -> DispatchResultWithPostInfo {
            let responder = ensure_response(<T as Config>::RuntimeOrigin::from(origin.clone()))
                .or_else(|_| {
                    T::UpdateOrigin::ensure_origin(origin).map(|_| MultiLocation::here())
                })?;
            if let Response::ExecutionResult(res) = response {
                if let Some(request) = Self::xcm_request(query_id) {
                    Self::do_notification_received(query_id, request, res)?;
                }

                Self::deposit_event(Event::<T>::NotificationReceived(
                    Box::new(responder),
                    query_id,
                    res,
                ));
            }
            Ok(().into())
        }

        /// Refund contributions
        #[pallet::call_index(17)]
        #[pallet::weight(<T as Config>::WeightInfo::refund())]
        #[transactional]
        pub fn refund(
            origin: OriginFor<T>,
            crowdloan: ParaId,
            lease_start: LeasePeriod,
            lease_end: LeasePeriod,
        ) -> DispatchResult {
            use ChildStorageKind::*;
            ensure_origin!(RefundOrigin, origin)?;

            let mut refund_count = 0u32;
            let mut all_refunded = true;

            let mut vault = Self::vaults((&crowdloan, &lease_start, &lease_end))
                .ok_or(Error::<T>::VaultDoesNotExist)?;

            ensure!(
                vault.phase == VaultPhase::Closed || vault.phase == VaultPhase::Failed,
                Error::<T>::IncorrectVaultPhase
            );

            'outer: for kind in [Contributed, Flying, Pending] {
                for (who, (amount, _)) in Self::contribution_iterator(vault.trie_index, kind) {
                    if refund_count >= T::RemoveKeysLimit::get() {
                        all_refunded = false;
                        break 'outer;
                    }

                    refund_count += 1;

                    Self::do_refund_for(&who, &mut vault, kind, amount)?;
                }
            }

            Vaults::<T>::insert((&crowdloan, &lease_start, &lease_end), vault);

            if all_refunded {
                Self::deposit_event(Event::<T>::AllRefunded(crowdloan, (lease_start, lease_end)));
            } else {
                Self::deposit_event(Event::<T>::PartiallyRefunded(
                    crowdloan,
                    (lease_start, lease_end),
                ));
            }

            Ok(())
        }

        /// Dissolve vault
        #[pallet::call_index(18)]
        #[pallet::weight(<T as Config>::WeightInfo::dissolve_vault())]
        #[transactional]
        pub fn dissolve_vault(
            origin: OriginFor<T>,
            crowdloan: ParaId,
            lease_start: LeasePeriod,
            lease_end: LeasePeriod,
        ) -> DispatchResult {
            ensure_origin!(DissolveOrigin, origin)?;

            let mut vault = Self::vaults((&crowdloan, &lease_start, &lease_end))
                .ok_or(Error::<T>::VaultDoesNotExist)?;

            ensure!(
                vault.phase == VaultPhase::Closed
                    || vault.phase == VaultPhase::Failed
                    || vault.phase == VaultPhase::Expired,
                Error::<T>::IncorrectVaultPhase
            );

            let has_childstorage = Self::has_childstorage(&vault);
            ensure!(!has_childstorage, Error::<T>::NotReadyToDissolve);

            ensure!(
                Self::total_contribution(&mut vault)?.is_zero(),
                Error::<T>::NotReadyToDissolve
            );

            Vaults::<T>::remove((&crowdloan, &lease_start, &lease_end));

            if let Some(vault_id) = LeasesRegistry::<T>::get(crowdloan) {
                if vault_id == (lease_start, lease_end) {
                    LeasesRegistry::<T>::remove(crowdloan);
                }
            }

            Self::deposit_event(Event::<T>::VaultDissolved(
                crowdloan,
                (lease_start, lease_end),
            ));

            Ok(())
        }

        /// Refund contributions for single user
        ///
        /// Once relaychain is in vrf but parachain didn't update vrf in time.
        /// Contributions received during this period should be refund to users,
        /// especially for those succeeded parachains.
        #[pallet::call_index(19)]
        #[pallet::weight(<T as Config>::WeightInfo::refund_for())]
        #[transactional]
        pub fn refund_for(
            origin: OriginFor<T>,
            dest: <T::Lookup as StaticLookup>::Source,
            crowdloan: ParaId,
            kind: ChildStorageKind,
            #[pallet::compact] amount: BalanceOf<T>,
            lease_start: LeasePeriod,
            lease_end: LeasePeriod,
        ) -> DispatchResult {
            ensure_origin!(RefundOrigin, origin)?;

            let who = T::Lookup::lookup(dest)?;
            let mut vault = Self::vaults((&crowdloan, &lease_start, &lease_end))
                .ok_or(Error::<T>::VaultDoesNotExist)?;

            ensure!(
                vault.phase == VaultPhase::Closed,
                Error::<T>::IncorrectVaultPhase
            );

            let (contribution, _) = Self::contribution_get(vault.trie_index, &who, kind);
            ensure!(contribution >= amount, Error::<T>::InsufficientContribution);

            Self::do_refund_for(&who, &mut vault, kind, amount)?;

            Vaults::<T>::insert((&crowdloan, &lease_start, &lease_end), vault);

            Self::deposit_event(Event::<T>::UserRefunded(
                crowdloan,
                (lease_start, lease_end),
                who,
                kind,
                amount,
            ));

            Ok(())
        }

        /// Update crowdloans proxy address in relaychain
        #[pallet::call_index(20)]
        #[pallet::weight(<T as Config>::WeightInfo::update_proxy())]
        #[transactional]
        pub fn update_proxy(origin: OriginFor<T>, proxy_address: AccountIdOf<T>) -> DispatchResult {
            T::ProxyOrigin::ensure_origin(origin)?;
            log::trace!(
                target: "crowdloans::update_proxy",
                "pre-toggle. proxy_address: {:?}",
                proxy_address
            );
            ProxyAddress::<T>::put(proxy_address.clone());

            Self::deposit_event(Event::<T>::ProxyUpdated(proxy_address));

            Ok(())
        }

        /// Update crowdloans proxy address in relaychain
        #[pallet::call_index(21)]
        #[pallet::weight(<T as Config>::WeightInfo::update_leases_bonus())]
        #[transactional]
        pub fn update_leases_bonus(
            origin: OriginFor<T>,
            lease_start: LeasePeriod,
            lease_end: LeasePeriod,
            bonus_config: BonusConfig<BalanceOf<T>>,
        ) -> DispatchResult {
            ensure_origin!(UpdateOrigin, origin)?;
            ensure!(
                lease_start <= lease_end,
                Error::<T>::LastPeriodBeforeFirstPeriod
            );
            ensure!(bonus_config.check(), Error::<T>::WrongBonusConfig);

            LeasesBonus::<T>::insert((&lease_start, &lease_end), bonus_config);
            Self::deposit_event(Event::<T>::LeasesBonusUpdated(
                (lease_start, lease_end),
                bonus_config,
            ));
            Ok(())
        }
    }

    impl<T: Config> Pallet<T> {
        /// Crowdloans vault account
        pub fn account_id() -> T::AccountId {
            T::PalletId::get().into_account_truncating()
        }

        /// Parachain's sovereign account on relaychain
        pub fn para_account_id() -> T::AccountId {
            T::SelfParaId::get().into_account_truncating()
        }

        pub(crate) fn current_vault(crowdloan: ParaId) -> Option<Vault<T>> {
            Self::current_lease(crowdloan).and_then(|(lease_start, lease_end)| {
                Self::vaults((&crowdloan, &lease_start, &lease_end))
            })
        }

        pub(crate) fn total_contribution(
            vault: &Vault<T>,
        ) -> Result<BalanceOf<T>, ArithmeticError> {
            vault
                .contributed
                .checked_add(vault.flying)
                .and_then(|sum| sum.checked_add(vault.pending))
                .ok_or(ArithmeticError::Overflow)
        }

        fn notify_placeholder() -> <T as Config>::RuntimeCall {
            <T as Config>::RuntimeCall::from(Call::<T>::notification_received {
                query_id: Default::default(),
                response: Default::default(),
            })
        }

        /// Get and recalculate the user's contribution for the specified kind of child storage
        #[require_transactional]
        fn do_update_contribution(
            who: &AccountIdOf<T>,
            vault: &mut Vault<T>,
            amount: BalanceOf<T>,
            new_referral_code: Option<Vec<u8>>,
            arithmetic_kind: ArithmeticKind,
            child_storage_kind: ChildStorageKind,
        ) -> Result<Vec<u8>, DispatchError> {
            use ArithmeticKind::*;
            use ChildStorageKind::*;

            let (contribution, old_referral_code) =
                Self::contribution_get(vault.trie_index, who, child_storage_kind);
            let referral_code = new_referral_code.unwrap_or(old_referral_code);
            let new_contribution = match (child_storage_kind, arithmetic_kind) {
                (Pending, Addition) => {
                    vault.pending = vault
                        .pending
                        .checked_add(amount)
                        .ok_or(ArithmeticError::Overflow)?;
                    contribution
                        .checked_add(amount)
                        .ok_or(ArithmeticError::Overflow)?
                }
                (Pending, Subtraction) => {
                    vault.pending = vault
                        .pending
                        .checked_sub(amount)
                        .ok_or(ArithmeticError::Underflow)?;
                    contribution
                        .checked_sub(amount)
                        .ok_or(ArithmeticError::Underflow)?
                }
                (Flying, Addition) => {
                    vault.flying = vault
                        .flying
                        .checked_add(amount)
                        .ok_or(ArithmeticError::Overflow)?;
                    contribution
                        .checked_add(amount)
                        .ok_or(ArithmeticError::Overflow)?
                }
                (Flying, Subtraction) => {
                    vault.flying = vault
                        .flying
                        .checked_sub(amount)
                        .ok_or(ArithmeticError::Underflow)?;
                    contribution
                        .checked_sub(amount)
                        .ok_or(ArithmeticError::Underflow)?
                }
                (Contributed, Addition) => {
                    vault.contributed = vault
                        .contributed
                        .checked_add(amount)
                        .ok_or(ArithmeticError::Overflow)?;
                    contribution
                        .checked_add(amount)
                        .ok_or(ArithmeticError::Overflow)?
                }
                (Contributed, Subtraction) => {
                    vault.contributed = vault
                        .contributed
                        .checked_sub(amount)
                        .ok_or(ArithmeticError::Underflow)?;
                    contribution
                        .checked_sub(amount)
                        .ok_or(ArithmeticError::Underflow)?
                }
            };
            if new_contribution.is_zero() {
                Self::contribution_kill(vault.trie_index, who, child_storage_kind);
            } else {
                Self::contribution_put(
                    vault.trie_index,
                    who,
                    &new_contribution,
                    &referral_code,
                    child_storage_kind,
                );
            }

            Ok(referral_code)
        }

        #[require_transactional]
        fn do_migrate_contribution(
            who: &AccountIdOf<T>,
            vault: &mut Vault<T>,
            amount: BalanceOf<T>,
            src_child_storage_kind: ChildStorageKind,
            dst_child_storage_kind: ChildStorageKind,
        ) -> DispatchResult {
            let referral_code = Self::do_update_contribution(
                who,
                vault,
                amount,
                None,
                ArithmeticKind::Subtraction,
                src_child_storage_kind,
            )?;

            Self::do_update_contribution(
                who,
                vault,
                amount,
                Some(referral_code),
                ArithmeticKind::Addition,
                dst_child_storage_kind,
            )?;
            Ok(())
        }

        #[require_transactional]
        fn do_notification_received(
            query_id: QueryId,
            request: XcmRequest<T>,
            res: Option<(u32, XcmError)>,
        ) -> DispatchResult {
            let executed = res.is_none();

            match request {
                XcmRequest::Contribute {
                    crowdloan,
                    vault_id: (lease_start, lease_end),
                    who,
                    amount,
                    referral_code,
                } if executed => {
                    let mut vault = Self::vaults((&crowdloan, &lease_start, &lease_end))
                        .ok_or(Error::<T>::VaultDoesNotExist)?;
                    T::Assets::burn_from(T::RelayCurrency::get(), &Self::account_id(), amount)?;
                    Self::do_migrate_contribution(
                        &who,
                        &mut vault,
                        amount,
                        ChildStorageKind::Flying,
                        ChildStorageKind::Contributed,
                    )?;
                    Vaults::<T>::insert((&crowdloan, &lease_start, &lease_end), vault);

                    Self::deposit_event(Event::<T>::VaultContributed(
                        crowdloan,
                        (lease_start, lease_end),
                        who,
                        amount,
                        referral_code,
                    ));
                }
                XcmRequest::Contribute {
                    crowdloan,
                    vault_id: (lease_start, lease_end),
                    who,
                    amount,
                    ..
                } if !executed => {
                    let mut vault = Self::vaults((&crowdloan, &lease_start, &lease_end))
                        .ok_or(Error::<T>::VaultDoesNotExist)?;
                    T::Assets::transfer(
                        T::RelayCurrency::get(),
                        &Self::account_id(),
                        &who,
                        amount,
                        false,
                    )?;

                    Self::do_update_contribution(
                        &who,
                        &mut vault,
                        amount,
                        None,
                        ArithmeticKind::Subtraction,
                        ChildStorageKind::Flying,
                    )?;
                    Vaults::<T>::insert((&crowdloan, &lease_start, &lease_end), vault);
                }
                XcmRequest::Withdraw {
                    crowdloan,
                    vault_id: (lease_start, lease_end),
                    amount: _,
                    target_phase,
                } if executed => {
                    let mut vault = Self::vaults((&crowdloan, &lease_start, &lease_end))
                        .ok_or(Error::<T>::VaultDoesNotExist)?;
                    let pre_phase = sp_std::mem::replace(&mut vault.phase, target_phase);
                    Vaults::<T>::insert((&crowdloan, &lease_start, &lease_end), vault);
                    Self::deposit_event(Event::<T>::VaultPhaseUpdated(
                        crowdloan,
                        (lease_start, lease_end),
                        pre_phase,
                        target_phase,
                    ));
                }
                _ => {}
            }

            if executed {
                XcmRequests::<T>::remove(query_id);
            }

            Ok(())
        }

        #[require_transactional]
        fn try_mutate_vault<F>(crowdloan: ParaId, phase: VaultPhase, cb: F) -> DispatchResult
        where
            F: FnOnce(&mut Vault<T>) -> DispatchResult,
        {
            let mut vault = Self::current_vault(crowdloan).ok_or(Error::<T>::VaultDoesNotExist)?;
            ensure!(vault.phase == phase, Error::<T>::IncorrectVaultPhase);
            cb(&mut vault)?;
            Vaults::<T>::insert(
                (
                    &crowdloan,
                    &vault.lease_start.clone(),
                    &vault.lease_end.clone(),
                ),
                vault,
            );
            Ok(())
        }

        pub(crate) fn id_from_index(index: TrieIndex, kind: ChildStorageKind) -> child::ChildInfo {
            let mut buf = Vec::new();
            buf.extend_from_slice({
                match kind {
                    ChildStorageKind::Pending => b"crowdloan:pending",
                    ChildStorageKind::Flying => b"crowdloan:flying",
                    ChildStorageKind::Contributed => b"crowdloan:contributed",
                }
            });
            buf.extend_from_slice(&index.encode()[..]);
            child::ChildInfo::new_default(T::Hashing::hash(&buf[..]).as_ref())
        }

        pub(crate) fn contribution_put(
            index: TrieIndex,
            who: &T::AccountId,
            balance: &BalanceOf<T>,
            referral_code: &[u8],
            kind: ChildStorageKind,
        ) {
            who.using_encoded(|b| {
                child::put(
                    &Self::id_from_index(index, kind),
                    b,
                    &(balance, referral_code),
                )
            });
        }

        pub(crate) fn contribution_get(
            index: TrieIndex,
            who: &T::AccountId,
            kind: ChildStorageKind,
        ) -> (BalanceOf<T>, Vec<u8>) {
            who.using_encoded(|b| {
                child::get_or_default::<(BalanceOf<T>, Vec<u8>)>(
                    &Self::id_from_index(index, kind),
                    b,
                )
            })
        }

        pub(crate) fn contribution_kill(
            index: TrieIndex,
            who: &T::AccountId,
            kind: ChildStorageKind,
        ) {
            who.using_encoded(|b| child::kill(&Self::id_from_index(index, kind), b));
        }

        fn contribution_iterator(
            index: TrieIndex,
            kind: ChildStorageKind,
        ) -> ChildTriePrefixIterator<(T::AccountId, (BalanceOf<T>, Vec<u8>))> {
            ChildTriePrefixIterator::<_>::with_prefix_over_key::<Identity>(
                &Self::id_from_index(index, kind),
                &[],
            )
        }

        #[require_transactional]
        fn do_contribute(
            who: &AccountIdOf<T>,
            crowdloan: ParaId,
            vault_id: VaultId,
            contribution_strategy: ContributionStrategy,
            amount: BalanceOf<T>,
            referral_code: Vec<u8>,
        ) -> Result<(), DispatchError> {
            let query_id = match contribution_strategy {
                ContributionStrategy::XCM => {
                    T::XCM::do_contribute(crowdloan, amount, who, Self::notify_placeholder())?
                }
                ContributionStrategy::XCMPROXY => {
                    let proxy_address =
                        Self::proxy_address().ok_or(Error::<T>::EmptyProxyAddress)?;
                    T::XCM::do_proxy_contribute(
                        crowdloan,
                        amount,
                        &proxy_address,
                        Self::notify_placeholder(),
                    )?
                }
            };

            XcmRequests::<T>::insert(
                query_id,
                XcmRequest::Contribute {
                    crowdloan,
                    vault_id,
                    who: who.clone(),
                    amount,
                    referral_code: referral_code.clone(),
                },
            );

            Self::deposit_event(Event::<T>::VaultDoContributing(
                crowdloan,
                vault_id,
                who.clone(),
                amount,
                referral_code,
            ));

            Ok(())
        }

        #[require_transactional]
        fn do_withdraw(
            crowdloan: ParaId,
            vault_id: VaultId,
            amount: BalanceOf<T>,
            target_phase: VaultPhase,
        ) -> Result<(), DispatchError> {
            log::trace!(
                target: "crowdloans::do_withdraw",
                "para_id: {:?}, amount: {:?}",
                &crowdloan,
                &amount,
            );

            let query_id = T::XCM::do_withdraw(
                crowdloan,
                Self::para_account_id(),
                Self::notify_placeholder(),
            )?;

            XcmRequests::<T>::insert(
                query_id,
                XcmRequest::Withdraw {
                    crowdloan,
                    vault_id,
                    amount,
                    target_phase,
                },
            );

            Self::deposit_event(Event::<T>::VaultDoWithdrawing(
                crowdloan,
                vault_id,
                amount,
                target_phase,
            ));
            Ok(())
        }

        // Return true if any childstorage has contribution.
        fn has_childstorage(vault: &Vault<T>) -> bool {
            use ChildStorageKind::*;
            [Contributed, Flying, Pending].iter().any(|&kind| {
                !Self::contribution_iterator(vault.trie_index, kind)
                    .count()
                    .is_zero()
            })
        }

        #[require_transactional]
        fn do_claim_for(
            who: T::AccountId,
            crowdloan: ParaId,
            lease_start: LeasePeriod,
            lease_end: LeasePeriod,
        ) -> DispatchResult {
            let ctoken = Self::ctoken_of((&lease_start, &lease_end))
                .ok_or(Error::<T>::CTokenDoesNotExist)?;
            let vault = Self::vaults((&crowdloan, &lease_start, &lease_end))
                .ok_or(Error::<T>::VaultDoesNotExist)?;

            ensure!(
                vault.phase == VaultPhase::Succeeded,
                Error::<T>::IncorrectVaultPhase
            );

            let (amount, _) =
                Self::contribution_get(vault.trie_index, &who, ChildStorageKind::Contributed);
            ensure!(!amount.is_zero(), Error::<T>::NoContributions);

            log::trace!(
                target: "crowdloans::claim",
                "who: {:?}, ctoken: {:?}, amount: {:?}, para_id: {:?}, lease_start: {:?}, lease_end: {:?}",
                &who,
                &ctoken,
                &amount,
                &crowdloan,
                &lease_start,
                &lease_end
            );

            T::Assets::mint_into(ctoken, &who, amount)?;

            Self::contribution_kill(vault.trie_index, &who, ChildStorageKind::Contributed);

            // Bonus for PARA, Not applicable for HKO
            let bonus_config = Self::leases_bonus((&lease_start, &lease_end));
            let bonus_amount = amount.saturating_mul(bonus_config.bonus_per_token);
            let normalized_amount = Self::normalized_amount(bonus_amount).unwrap_or_default();
            if !normalized_amount.is_zero() {
                T::Streaming::create(
                    Self::account_id(),
                    who.clone(),
                    normalized_amount,
                    T::GetNativeCurrencyId::get(),
                    bonus_config.start_time,
                    bonus_config.end_time,
                    false,
                )?;
            }

            Self::deposit_event(Event::<T>::VaultClaimed(
                crowdloan,
                (lease_start, lease_end),
                ctoken,
                who,
                amount,
                VaultPhase::Succeeded,
            ));

            Ok(())
        }

        pub(crate) fn normalized_amount(amount: BalanceOf<T>) -> Option<BalanceOf<T>> {
            use Ordering::*;
            let relay_decimal = T::Decimal::get_decimal(&T::RelayCurrency::get())?;
            let native_decimal = T::Decimal::get_decimal(&T::GetNativeCurrencyId::get())?;
            match relay_decimal.cmp(&native_decimal) {
                Less => {
                    amount.checked_mul(10u128.checked_pow((native_decimal - relay_decimal).into())?)
                }
                Equal => Some(amount),
                Greater => {
                    amount.checked_div(10u128.checked_pow((relay_decimal - native_decimal).into())?)
                }
            }
        }

        #[require_transactional]
        fn do_withdraw_for(
            who: T::AccountId,
            crowdloan: ParaId,
            lease_start: LeasePeriod,
            lease_end: LeasePeriod,
        ) -> DispatchResult {
            let mut vault = Self::vaults((&crowdloan, &lease_start, &lease_end))
                .ok_or(Error::<T>::VaultDoesNotExist)?;

            ensure!(
                vault.phase == VaultPhase::Failed,
                Error::<T>::IncorrectVaultPhase
            );

            let (amount, _) =
                Self::contribution_get(vault.trie_index, &who, ChildStorageKind::Contributed);
            ensure!(!amount.is_zero(), Error::<T>::NoContributions);

            log::trace!(
                target: "crowdloans::withdraw",
                "who: {:?}, amount: {:?}, para_id: {:?}, lease_start: {:?}, lease_end: {:?}",
                &who,
                &amount,
                &crowdloan,
                &lease_start,
                &lease_end
            );

            Self::contribution_kill(vault.trie_index, &who, ChildStorageKind::Contributed);

            vault.contributed = vault
                .contributed
                .checked_sub(amount)
                .ok_or(ArithmeticError::Underflow)?;

            // SovereignAccount on relaychain must have
            // withdrawn the contribution
            T::Assets::mint_into(T::RelayCurrency::get(), &who, amount)?;

            Vaults::<T>::insert((&crowdloan, &lease_start, &lease_end), vault);

            Self::deposit_event(Event::<T>::VaultWithdrew(
                crowdloan,
                (lease_start, lease_end),
                who,
                amount,
                VaultPhase::Failed,
            ));

            Ok(())
        }

        #[require_transactional]
        fn do_refund_for(
            who: &T::AccountId,
            vault: &mut Vault<T>,
            kind: ChildStorageKind,
            amount: BalanceOf<T>,
        ) -> DispatchResult {
            let relay_currency = T::RelayCurrency::get();

            if kind == ChildStorageKind::Contributed {
                // SovereignAccount on relaychain must have
                // withdrawn the contribution
                T::Assets::mint_into(relay_currency, who, amount)?;
            } else {
                T::Assets::transfer(relay_currency, &Self::account_id(), who, amount, false)?;
            }

            Self::do_update_contribution(
                who,
                vault,
                amount,
                None,
                ArithmeticKind::Subtraction,
                kind,
            )?;
            Ok(())
        }

        // just iterate now and require improve later when CTokensRegistry increased
        fn find_vault_by_asset_id(asset_id: &AssetIdOf<T>) -> Option<(AssetIdOf<T>, AssetIdOf<T>)> {
            for (vault, ctoken_id) in CTokensRegistry::<T>::iter() {
                if &ctoken_id == asset_id {
                    return Some(vault);
                }
            }
            None
        }

        fn get_vault_term_rate(
            (start_lease, end_lease): (LeasePeriod, LeasePeriod),
        ) -> Option<(Rate, Rate)> {
            let current_block = T::RelayChainBlockNumberProvider::current_block_number();
            if current_block == T::BlockNumber::zero() {
                return None;
            }
            let lease_period = T::LeasePeriod::get();
            let start_block = lease_period
                .saturating_mul(start_lease.into())
                .saturating_add(T::LeaseOffset::get());
            let end_block = lease_period
                .saturating_mul((end_lease + 1).into())
                .saturating_add(T::LeaseOffset::get());
            let lease_length = lease_period.saturating_mul((end_lease - start_lease + 1).into());
            let blocks_per_year = T::LeasePerYear::get().saturating_mul(lease_period);
            let total_term_by_year = Rate::saturating_from_rational(
                lease_length.saturated_into::<u32>(),
                blocks_per_year.saturated_into::<u32>(),
            );
            let term_rate: Rate;
            if current_block < start_block {
                term_rate = Rate::zero();
            } else if current_block >= start_block && current_block < end_block {
                term_rate = Rate::saturating_from_rational(
                    current_block
                        .saturating_sub(start_block)
                        .saturated_into::<u32>(),
                    lease_length.saturated_into::<u32>(),
                );
            } else {
                term_rate = Rate::one();
            }
            Some((term_rate, total_term_by_year))
        }
    }

    impl<T: Config> VaultTokenExchangeRateProvider<AssetIdOf<T>> for Pallet<T> {
        /// 1/(1+r)^T
        /// T is the remaining term-to-maturity with year as unit
        /// r is the implied yield rate
        fn get_exchange_rate(asset_id: &AssetIdOf<T>, start_exchange_rate: Rate) -> Option<Rate> {
            Self::find_vault_by_asset_id(asset_id)
                .and_then(|vault| Self::get_vault_term_rate(vault))
                .and_then(|(term_rate, total_term_by_year)| {
                    let remaining_year = fixed_u128_to_float(total_term_by_year)
                        * (1_f64 - fixed_u128_to_float(term_rate));
                    let current_rate = power_float(
                        1_f64 + fixed_u128_to_float(start_exchange_rate),
                        remaining_year,
                    )
                    .ok()?;
                    fixed_u128_from_float(current_rate as f64).reciprocal()
                })
        }
    }

    impl<T: Config> VaultTokenCurrenciesFilter<AssetIdOf<T>> for Pallet<T> {
        fn contains(asset_id: &AssetIdOf<T>) -> bool {
            Self::find_vault_by_asset_id(asset_id).is_some()
        }
    }
}
