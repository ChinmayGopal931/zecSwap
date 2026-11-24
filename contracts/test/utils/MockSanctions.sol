// SPDX-License-Identifier: Apache-2.0
pragma solidity 0.8.28;

import {ISanctions} from "../../src/Deposits.sol";

contract MockSanctions is ISanctions {
    mapping(address => bool) public override isSanctioned;

    function sanction(address account) external {
        isSanctioned[account] = true;
    }
}
