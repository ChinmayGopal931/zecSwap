// SPDX-License-Identifier: Apache-2.0
pragma solidity 0.8.28;

import {IERC20} from "../../src/Token.sol";
import {SimRailgun, Transaction, ShieldRequest} from "./SimRailgun.sol";

contract SimRelayAdapt {
    struct Call {
        address to;
        bytes data;
        uint256 value;
    }

    struct ActionData {
        bytes31 random;
        bool requireSuccess;
        uint256 minGasLimit;
        Call[] calls;
    }

    event CallError(uint256 callIndex, bytes revertReason);

    error CallFailed(uint256 callIndex, bytes revertReason);

    SimRailgun public immutable railgun;
    bool private executing;

    constructor(SimRailgun railgun_) {
        railgun = railgun_;
    }

    modifier onlySelf() {
        require(msg.sender == address(this), "RelayAdapt: External call to onlySelf function");
        _;
    }

    modifier onlySelfIfExecuting() {
        require(!executing || msg.sender == address(this), "RelayAdapt: External call to onlySelf function");
        executing = true;
        _;
        executing = false;
    }

    function relay(Transaction[] calldata _transactions, ActionData calldata _actionData)
        external
        payable
        onlySelfIfExecuting
    {
        require(gasleft() > _actionData.minGasLimit, "RelayAdapt: Not enough gas supplied");
        bytes32 expected = getAdaptParams(_transactions, _actionData);
        for (uint256 i; i < _transactions.length; ++i) {
            require(
                _transactions[i].boundParams.adaptParams == expected, "RelayAdapt: AdaptID Parameters Mismatch"
            );
        }
        railgun.transact(_transactions);
        for (uint256 i; i < _actionData.calls.length; ++i) {
            Call calldata call = _actionData.calls[i];
            require(call.to != address(railgun), "RelayAdapt: Refusing to call Railgun contract");
            (bool ok, bytes memory returned) = call.to.call{value: call.value}(call.data);
            if (ok) continue;
            emit CallError(i, returned);
            if (_actionData.requireSuccess) revert CallFailed(i, returned);
        }
    }

    function getAdaptParams(Transaction[] calldata _transactions, ActionData calldata _actionData)
        public
        pure
        returns (bytes32)
    {
        bytes32[][] memory spent = new bytes32[][](_transactions.length);
        for (uint256 i; i < _transactions.length; ++i) {
            spent[i] = _transactions[i].nullifiers;
        }
        return keccak256(abi.encode(spent, _transactions.length, _actionData));
    }

    function shield(ShieldRequest[] calldata _shieldRequests) external onlySelf {
        uint256 valid;
        uint120[] memory values = new uint120[](_shieldRequests.length);
        for (uint256 i; i < _shieldRequests.length; ++i) {
            require(_shieldRequests[i].preimage.token.tokenType == 0, "RelayAdapt: Unhandled token type");
            IERC20 token = IERC20(_shieldRequests[i].preimage.token.tokenAddress);
            values[i] = _shieldRequests[i].preimage.value;
            if (values[i] == 0) values[i] = uint120(token.balanceOf(address(this)));
            if (values[i] == 0) continue;
            valid += 1;
            require(token.approve(address(railgun), 0), "RelayAdapt: approve failed");
            require(token.approve(address(railgun), values[i]), "RelayAdapt: approve failed");
        }
        ShieldRequest[] memory shields = new ShieldRequest[](valid);
        uint256 at;
        for (uint256 i; i < _shieldRequests.length; ++i) {
            if (values[i] == 0) continue;
            shields[at] = _shieldRequests[i];
            shields[at].preimage.value = values[i];
            at += 1;
        }
        if (valid > 0) railgun.shield(shields);
    }

    receive() external payable {}
}
