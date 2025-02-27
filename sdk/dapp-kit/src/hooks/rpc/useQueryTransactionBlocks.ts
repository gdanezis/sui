// Copyright (c) Mysten Labs, Inc.
// SPDX-License-Identifier: Apache-2.0

/**
 *  ######################################
 *  ### DO NOT EDIT THIS FILE DIRECTLY ###
 *  ######################################
 *
 * This file is generated from:
 * /crates/sui-open-rpc/spec/openrpc.json
 */

import type { QueryTransactionBlocksParams } from '@mysten/sui.js/client';

import type { UseSuiClientInfiniteQueryOptions } from '../useSuiClientInfiniteQuery.js';
import { useSuiClientInfiniteQuery } from '../useSuiClientInfiniteQuery.js';
import type { UseSuiClientQueryOptions } from '../useSuiClientQuery.js';
import { useSuiClientQuery } from '../useSuiClientQuery.js';

export function useQueryTransactionBlocks(
	params: QueryTransactionBlocksParams,
	options?: UseSuiClientQueryOptions<'queryTransactionBlocks'>,
) {
	return useSuiClientQuery(
		{
			method: 'queryTransactionBlocks',
			params,
		},
		options,
	);
}

export function useQueryTransactionBlocksInfinite(
	params: QueryTransactionBlocksParams,
	options?: UseSuiClientInfiniteQueryOptions<'queryTransactionBlocks'>,
) {
	return useSuiClientInfiniteQuery(
		{
			method: 'queryTransactionBlocks',
			params,
		},
		options,
	);
}
