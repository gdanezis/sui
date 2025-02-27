// Copyright (c) Mysten Labs, Inc.
// SPDX-License-Identifier: Apache-2.0

import { TransactionArgument, type TransactionBlock } from '@mysten/sui.js/transactions';

import {
	attachFloorPriceRuleTx,
	attachKioskLockRuleTx,
	attachPersonalKioskRuleTx,
	attachRoyaltyRuleTx,
} from '../tx/rules/attach';
import {
	createTransferPolicy,
	createTransferPolicyWithoutSharing,
	removeTransferPolicyRule,
	shareTransferPolicy,
	withdrawFromPolicy,
} from '../tx/transfer-policy';
import { TransferPolicyCap, type ObjectArgument } from '../types';
import { KioskClient } from './kiosk-client';

export type TransferPolicyBaseParams = {
	type: string;
	publisher: ObjectArgument;
	skipCheck?: boolean;
};

export type TransferPolicyTransactionParams = {
	kioskClient: KioskClient;
	transactionBlock: TransactionBlock;
	cap?: TransferPolicyCap;
};

export class TransferPolicyTransaction {
	transactionBlock: TransactionBlock;
	kioskClient: KioskClient;
	policy?: ObjectArgument;
	policyCap?: ObjectArgument;
	type?: string;

	constructor({ kioskClient, transactionBlock, cap }: TransferPolicyTransactionParams) {
		this.kioskClient = kioskClient;
		this.transactionBlock = transactionBlock;
		if (cap) this.setCap(cap);
	}

	/**
	 * A function to create a new transfer policy.
	 * Checks if there's already an existing transfer policy to prevent
	 * double transfer polciy mistakes.
	 * There's an optional `skipCheck` flag that will just create the policy
	 * without checking
	 *
	 * @param type The Type (`T`) for which we're creating the transfer policy.
	 * @param publisher The Publisher Object Id.
	 * @param address Address to save the `TransferPolicyCap` object to.
	 * @param skipCheck (Optional) skip checking if a transfer policy already exists
	 */
	async createAndShare({
		type,
		publisher,
		address,
		skipCheck,
	}: TransferPolicyBaseParams & {
		address: string;
	}) {
		if (!skipCheck) {
			const policies = await this.kioskClient.getTransferPolicies({ type });
			if (policies.length > 0) throw new Error("There's already transfer policy for this Type.");
		}
		const cap = createTransferPolicy(this.transactionBlock, type, publisher);
		this.transactionBlock.transferObjects([cap], this.transactionBlock.pure(address, 'address'));
	}

	/**
	 * A convenient function to create a Transfer Policy and attach some rules
	 * before sharing it (so you can prepare it in a single PTB)
	 * @param type The Type (`T`) for which we're creating the transfer policy.
	 * @param publisher The Publisher Object Id.
	 * @param address Address to save the `TransferPolicyCap` object to.
	 * @param skipCheck (Optional) skip checking if a transfer policy already exists
	 */
	async create({
		type,
		publisher,
		skipCheck,
	}: TransferPolicyBaseParams): Promise<TransferPolicyTransaction> {
		if (!skipCheck) {
			const policies = await this.kioskClient.getTransferPolicies({ type });
			if (policies.length > 0) throw new Error("There's already transfer policy for this Type.");
		}
		const [policy, policyCap] = createTransferPolicyWithoutSharing(
			this.transactionBlock,
			type,
			publisher,
		);

		this.#setup(policy, policyCap, type); // sets the client's TP to the newly created one.
		return this;
	}

	/**
	 * This can be called after calling the `create` function to share the `TransferPolicy`,
	 * and transfer the `TransferPolicyCap` to the specified address
	 *
	 * @param address The address to transfer the `TransferPolicyCap`
	 */
	shareAndTransferCap(address: string) {
		if (!this.type || !this.policyCap || !this.policy)
			throw new Error('This function can only be called after `transferPolicyManager.create`');

		shareTransferPolicy(this.transactionBlock, this.type, this.policy as TransactionArgument);
		this.transactionBlock.transferObjects(
			[this.policyCap as TransactionArgument],
			this.transactionBlock.pure(address, 'address'),
		);
	}

	/**
	 * Setup the TransferPolicy by passing a `cap` returned from `kioskClient.getOwnedTransferPolicies` or
	 * `kioskClient.getOwnedTransferPoliciesByType`.
	 * @param policyCapId The `TransferPolicyCap`
	 */
	setCap({ policyId, policyCapId, type }: TransferPolicyCap) {
		return this.#setup(policyId, policyCapId, type);
	}

	/**
	 * Withdraw from the transfer policy's profits.
	 * @param address Address to transfer the profits to.
	 * @param amount (Optional) amount parameter. Will withdraw all profits if the amount is not specified.
	 */
	withdraw(address: string, amount?: string | bigint) {
		this.#validateInputs();
		// Withdraw coin for specified amount (or none)
		const coin = withdrawFromPolicy(
			this.transactionBlock,
			this.type!,
			this.policy!,
			this.policyCap!,
			amount,
		);

		this.transactionBlock.transferObjects([coin], this.transactionBlock.pure(address, 'address'));

		return this;
	}

	/**
	 *  Adds the Kiosk Royalty rule to the Transfer Policy.
	 *  You can pass the percentage, as well as a minimum amount.
	 *  The royalty that will be paid is the MAX(percentage, minAmount).
	 * 	You can pass 0 in either value if you want only percentage royalty, or a fixed amount fee.
	 * 	(but you should define at least one of them for the rule to make sense).
	 *
	 * 	@param percentageBps The royalty percentage in basis points. Use `percentageToBasisPoints` helper to convert from percentage [0,100].
	 * 	@param minAmount The minimum royalty amount per request in MIST.
	 */
	addRoyaltyRule(
		percentageBps: number | string, // this is in basis points.
		minAmount: number | string,
	) {
		this.#validateInputs();

		// Hard-coding package Ids as these don't change.
		// Also, it's hard to keep versioning as with network wipes, mainnet
		// and testnet will conflict.
		attachRoyaltyRuleTx(
			this.transactionBlock,
			this.type!,
			this.policy!,
			this.policyCap!,
			percentageBps,
			minAmount,
			this.kioskClient.getRulePackageId('royaltyRulePackageId'),
		);
		return this;
	}

	/**
	 * Adds the Kiosk Lock Rule to the Transfer Policy.
	 * This Rule forces buyer to lock the item in the kiosk, preserving strong royalties.
	 */
	addLockRule() {
		this.#validateInputs();

		attachKioskLockRuleTx(
			this.transactionBlock,
			this.type!,
			this.policy!,
			this.policyCap!,
			this.kioskClient.getRulePackageId('kioskLockRulePackageId'),
		);
		return this;
	}

	/**
	 * Attaches the Personal Kiosk Rule, making a purchase valid only for `SoulBound` kiosks.
	 */
	addPersonalKioskRule() {
		this.#validateInputs();

		attachPersonalKioskRuleTx(
			this.transactionBlock,
			this.type!,
			this.policy!,
			this.policyCap!,
			this.kioskClient.getRulePackageId('personalKioskRulePackageId'),
		);
		return this;
	}

	/**
	 * A function to add the floor price rule to a transfer policy.
	 * @param minPrice The minimum price in MIST.
	 */
	addFloorPriceRule(minPrice: string | bigint) {
		this.#validateInputs();

		attachFloorPriceRuleTx(
			this.transactionBlock,
			this.type!,
			this.policy!,
			this.policyCap!,
			minPrice,
			this.kioskClient.getRulePackageId('floorPriceRulePackageId'),
		);
		return this;
	}

	/**
	 * Generic helper to remove a rule, not from the SDK's base ruleset.
	 * @param ruleType The Rule Type
	 * @param configType The Config Type
	 */
	removeRule({ ruleType, configType }: { ruleType: string; configType: string }) {
		this.#validateInputs();

		removeTransferPolicyRule(
			this.transactionBlock,
			this.type!,
			ruleType,
			configType,
			this.policy!,
			this.policyCap!,
		);
	}

	/**
	 * Removes the lock rule.
	 */
	removeLockRule() {
		this.#validateInputs();

		const packageId = this.kioskClient.getRulePackageId('kioskLockRulePackageId');

		removeTransferPolicyRule(
			this.transactionBlock,
			this.type!,
			`${packageId}::kiosk_lock_rule::Rule`,
			`${packageId}::kiosk_lock_rule::Config`,
			this.policy!,
			this.policyCap!,
		);
		return this;
	}

	/**
	 * Removes the Royalty rule
	 */
	removeRoyaltyRule() {
		this.#validateInputs();

		const packageId = this.kioskClient.getRulePackageId('royaltyRulePackageId');

		removeTransferPolicyRule(
			this.transactionBlock,
			this.type!,
			`${packageId}::royalty_rule::Rule`,
			`${packageId}::royalty_rule::Config`,
			this.policy!,
			this.policyCap!,
		);
		return this;
	}

	removePersonalKioskRule() {
		this.#validateInputs();

		const packageId = this.kioskClient.getRulePackageId('personalKioskRulePackageId');

		removeTransferPolicyRule(
			this.transactionBlock,
			this.type!,
			`${packageId}::personal_kiosk_rule::Rule`,
			`bool`,
			this.policy!,
			this.policyCap!,
		);
		return this;
	}

	removeFloorPriceRule() {
		this.#validateInputs();

		const packageId = this.kioskClient.getRulePackageId('floorPriceRulePackageId');

		removeTransferPolicyRule(
			this.transactionBlock,
			this.type!,
			`${packageId}::floor_price_rule::Rule`,
			`${packageId}::floor_price_rule::Config`,
			this.policy!,
			this.policyCap!,
		);
		return this;
	}

	getPolicy() {
		if (!this.policy) throw new Error('Policy not set.');
		return this.policy;
	}

	getPolicyCap() {
		if (!this.policyCap) throw new Error('Transfer Policy Cap not set.');
		return this.policyCap;
	}

	// Internal function that that the policy's Id + Cap + type have been set.
	#validateInputs() {
		const genericErrorMessage = `Please use 'setCap()' to setup the TransferPolicy.`;
		if (!this.policy) throw new Error(`${genericErrorMessage} Missing: Transfer Policy Object.`);
		if (!this.policyCap)
			throw new Error(`${genericErrorMessage} Missing: TransferPolicyCap Object ID`);
		if (!this.type)
			throw new Error(
				`${genericErrorMessage} Missing: Transfer Policy object type (e.g. {packageId}::item::Item)`,
			);
	}

	/**
	 * Setup the state of the TransferPolicyTransaction.
	 */
	#setup(policyId: ObjectArgument, policyCap: ObjectArgument, type: string) {
		this.policy = policyId;
		this.policyCap = policyCap;
		this.type = type;
		return this;
	}
}
