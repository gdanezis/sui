// Copyright (c) Mysten Labs, Inc.
// SPDX-License-Identifier: Apache-2.0

import { type Keypair } from '@mysten/sui.js/cryptography';

import { AccountType, type Account } from './Account';
import { AccountKeypair } from './AccountKeypair';

/**
 * @deprecated
 */
export type SerializedImportedAccount = {
	type: AccountType.IMPORTED;
	address: string;
	derivationPath: null;
};

/**
 * @deprecated
 */
export class ImportedAccount implements Account {
	readonly accountKeypair: AccountKeypair;
	readonly type: AccountType;
	readonly address: string;

	constructor({ keypair }: { keypair: Keypair }) {
		this.type = AccountType.IMPORTED;
		this.accountKeypair = new AccountKeypair(keypair);
		this.address = this.accountKeypair.publicKey.toSuiAddress();
	}

	toJSON(): SerializedImportedAccount {
		return {
			type: AccountType.IMPORTED,
			address: this.address,
			derivationPath: null,
		};
	}

	getPublicKey() {
		return this.accountKeypair.publicKey.toBase64();
	}
}
