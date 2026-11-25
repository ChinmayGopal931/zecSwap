// SPDX-License-Identifier: Apache-2.0
pragma solidity 0.8.28;

import {Script, console} from "forge-std/Script.sol";

import {IRailgun} from "../src/ShieldVault.sol";
import {ZecSwap} from "../src/ZecSwap.sol";
import {TestToken} from "../test/utils/TestToken.sol";

contract Deploy is Script {
    function run() external returns (ZecSwap swaps, TestToken token) {
        uint256 minWindow = vm.envOr("MIN_WINDOW", uint256(1 hours));
        uint256 lateWindow = vm.envOr("LATE_WINDOW", uint256(24 hours));
        IRailgun railgun = IRailgun(vm.envOr("RAILGUN", address(0)));
        uint256 key = vm.envOr("DEPLOYER_PRIVATE_KEY", uint256(0));
        if (key == 0) vm.startBroadcast();
        else vm.startBroadcast(key);
        swaps = new ZecSwap(minWindow, lateWindow, railgun);
        if (vm.envOr("DEPLOY_TEST_TOKEN", false)) {
            token = new TestToken();
        }
        vm.stopBroadcast();
        console.log("ZecSwap", address(swaps));
        console.log("TestToken", address(token));
    }
}
