#!/bin/bash

trap 'error "An error occurred. Check the logs above for details"; exit 1' ERR

# change the variables based on the testnet environment
PRECONF_WHITELIST=0x7Df5C00013E42874E6c1fd4cB4396bfff5F18E91
RPC_URL=http://remotebeast:48545

OPERATOR_COUNT_HEX=$(cast call --rpc-url $RPC_URL $PRECONF_WHITELIST "operatorCount()")
OPERATOR_COUNT=0

# Check if the returned value is not "0x" and convert it to decimal
if [ "$OPERATOR_COUNT_HEX" != "0x" ]; then
    OPERATOR_COUNT=$(printf "%d" $OPERATOR_COUNT_HEX)
fi

echo "found $OPERATOR_COUNT operators"

# Loop through all operators and print their addresses
for ((i = 0; i < $OPERATOR_COUNT; i++)); do
    # NOTE: when the WL upgrades to the Pacaya impl, use "operatorMapping(uint256)(address)" instead.
    # Ref: https://github.com/taikoxyz/taiko-mono/commit/c8066151fa2f18679818232b13a32a13d6fd475b#diff-852303900e703833c4b2842b78f5f41bd66fb8ee009787cb692628e94b259c0cR13-R17
    OPERATOR_ADDRESS=$(cast call --rpc-url $RPC_URL $PRECONF_WHITELIST "operatorIndexToOperator(uint256)(address)" $i)
    echo "index: $i, address: $OPERATOR_ADDRESS"
done
