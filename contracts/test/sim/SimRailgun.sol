// SPDX-License-Identifier: Apache-2.0
pragma solidity 0.8.28;

import {IERC20} from "../../src/Token.sol";

struct G1Point {
    uint256 x;
    uint256 y;
}

struct G2Point {
    uint256[2] x;
    uint256[2] y;
}

struct SnarkProof {
    G1Point a;
    G2Point b;
    G1Point c;
}

struct CommitmentCiphertext {
    bytes32[4] ciphertext;
    bytes32 blindedSenderViewingKey;
    bytes32 blindedReceiverViewingKey;
    bytes annotationData;
    bytes memo;
}

struct BoundParams {
    uint16 treeNumber;
    uint72 minGasPrice;
    uint8 unshield;
    uint64 chainID;
    address adaptContract;
    bytes32 adaptParams;
    CommitmentCiphertext[] commitmentCiphertext;
}

struct TokenData {
    uint8 tokenType;
    address tokenAddress;
    uint256 tokenSubID;
}

struct CommitmentPreimage {
    bytes32 npk;
    TokenData token;
    uint120 value;
}

struct ShieldCiphertext {
    bytes32[3] encryptedBundle;
    bytes32 shieldKey;
}

struct ShieldRequest {
    CommitmentPreimage preimage;
    ShieldCiphertext ciphertext;
}

struct Transaction {
    SnarkProof proof;
    bytes32 merkleRoot;
    bytes32[] nullifiers;
    bytes32[] commitments;
    BoundParams boundParams;
    CommitmentPreimage unshieldPreimage;
}

contract SimRailgun {
    event Shield(
        uint256 treeNumber,
        uint256 startPosition,
        CommitmentPreimage[] commitments,
        ShieldCiphertext[] shieldCiphertext,
        uint256[] fees
    );
    event Transact(uint256 treeNumber, uint256 startPosition, bytes32[] hash, CommitmentCiphertext[] ciphertext);
    event Unshield(address to, TokenData token, uint256 amount, uint256 fee);
    event Nullified(uint16 treeNumber, bytes32[] nullifier);

    uint120 public constant FEE_BPS = 25;
    uint256 public constant MAX_NOTES = 13;
    bytes32 public constant GENESIS_ROOT = keccak256("zecswap-sim");

    address public immutable treasury = address(0x7ea5);
    address public immutable prover;
    mapping(uint256 tree => mapping(bytes32 nullifier => bool)) public nullifiers;
    mapping(uint256 tree => mapping(bytes32 root => bool)) public rootHistory;
    bytes32 public merkleRoot;
    uint256 public nextLeafIndex;

    constructor(address prover_) {
        prover = prover_;
        merkleRoot = GENESIS_ROOT;
        rootHistory[0][GENESIS_ROOT] = true;
    }

    function shield(ShieldRequest[] calldata requests) external payable {
        CommitmentPreimage[] memory commitments = new CommitmentPreimage[](requests.length);
        ShieldCiphertext[] memory ciphertexts = new ShieldCiphertext[](requests.length);
        uint256[] memory fees = new uint256[](requests.length);
        bytes32[] memory hashes = new bytes32[](requests.length);
        for (uint256 i; i < requests.length; ++i) {
            CommitmentPreimage calldata preimage = requests[i].preimage;
            require(preimage.value > 0, "RailgunLogic: Invalid Note Value");
            require(preimage.token.tokenType == 0, "RailgunLogic: Invalid Token Type");
            uint120 fee = uint120(uint256(preimage.value) * FEE_BPS / 10_000);
            uint120 base = preimage.value - fee;
            IERC20 token = IERC20(preimage.token.tokenAddress);
            require(token.transferFrom(msg.sender, address(this), base), "RailgunLogic: transfer failed");
            require(token.transferFrom(msg.sender, treasury, fee), "RailgunLogic: fee transfer failed");
            commitments[i] = CommitmentPreimage(preimage.npk, preimage.token, base);
            ciphertexts[i] = requests[i].ciphertext;
            fees[i] = fee;
            hashes[i] = keccak256(abi.encode(commitments[i]));
        }
        emit Shield(0, nextLeafIndex, commitments, ciphertexts, fees);
        _insert(hashes);
    }

    function transact(Transaction[] calldata _transactions) external payable {
        uint256 outputs;
        for (uint256 i; i < _transactions.length; ++i) {
            outputs += _transactions[i].commitments.length;
            if (_transactions[i].boundParams.unshield != 0) outputs -= 1;
        }
        bytes32[] memory hashes = new bytes32[](outputs);
        CommitmentCiphertext[] memory ciphertexts = new CommitmentCiphertext[](outputs);
        uint256 at;
        for (uint256 i; i < _transactions.length; ++i) {
            Transaction calldata transaction = _transactions[i];
            _validate(transaction);
            uint16 tree = transaction.boundParams.treeNumber;
            for (uint256 j; j < transaction.nullifiers.length; ++j) {
                bytes32 nullifier = transaction.nullifiers[j];
                require(!nullifiers[tree][nullifier], "RailgunSmartWallet: Note already spent");
                nullifiers[tree][nullifier] = true;
            }
            emit Nullified(tree, transaction.nullifiers);
            uint256 count = transaction.commitments.length;
            if (transaction.boundParams.unshield != 0) count -= 1;
            for (uint256 j; j < count; ++j) {
                hashes[at] = transaction.commitments[j];
                ciphertexts[at] = transaction.boundParams.commitmentCiphertext[j];
                at += 1;
            }
        }
        for (uint256 i; i < _transactions.length; ++i) {
            if (_transactions[i].boundParams.unshield != 0) {
                _transferOut(_transactions[i].unshieldPreimage);
            }
        }
        if (outputs > 0) {
            emit Transact(0, nextLeafIndex, hashes, ciphertexts);
            _insert(hashes);
        }
    }

    function _validate(Transaction calldata transaction) private view {
        BoundParams calldata bound = transaction.boundParams;
        require(tx.gasprice >= bound.minGasPrice, "RailgunLogic: Gas price too low");
        require(
            bound.adaptContract == address(0) || bound.adaptContract == msg.sender,
            "RailgunLogic: Invalid Adapt Contract as Sender"
        );
        require(bound.chainID == block.chainid, "RailgunLogic: ChainID mismatch");
        require(rootHistory[bound.treeNumber][transaction.merkleRoot], "RailgunLogic: Invalid Merkle Root");
        require(bound.unshield <= 2, "RailgunLogic: Invalid Unshield Type");
        uint256 spent = transaction.nullifiers.length;
        uint256 made = transaction.commitments.length;
        require(spent > 0 && spent <= MAX_NOTES && made > 0 && made <= MAX_NOTES, "Verifier: Key not set");
        if (bound.unshield != 0) {
            require(bound.commitmentCiphertext.length == made - 1, "RailgunLogic: Invalid Note Ciphertext Array Length");
            CommitmentPreimage memory note = transaction.unshieldPreimage;
            if (bound.unshield == 2) note.npk = bytes32(uint256(uint160(msg.sender)));
            require(keccak256(abi.encode(note)) == transaction.commitments[made - 1], "RailgunLogic: Invalid Withdraw Note");
        } else {
            require(bound.commitmentCiphertext.length == made, "RailgunLogic: Invalid Note Ciphertext Array Length");
        }
        require(_verified(transaction), "RailgunLogic: Invalid Snark Proof");
    }

    function _verified(Transaction calldata transaction) private view returns (bool) {
        bytes32 digest = keccak256(
            abi.encode(
                transaction.merkleRoot,
                transaction.nullifiers,
                transaction.commitments,
                keccak256(abi.encode(transaction.boundParams))
            )
        );
        SnarkProof calldata proof = transaction.proof;
        if (proof.c.x != 27 && proof.c.x != 28) return false;
        return ecrecover(digest, uint8(proof.c.x), bytes32(proof.a.x), bytes32(proof.a.y)) == prover;
    }

    function _transferOut(CommitmentPreimage calldata note) private {
        require(note.token.tokenType == 0, "RailgunLogic: Invalid Token Type");
        address to = address(uint160(uint256(note.npk)));
        uint120 fee = uint120(uint256(note.value) * FEE_BPS / 10_000);
        uint120 base = note.value - fee;
        IERC20 token = IERC20(note.token.tokenAddress);
        require(token.transfer(to, base), "RailgunLogic: transfer failed");
        require(token.transfer(treasury, fee), "RailgunLogic: fee transfer failed");
        emit Unshield(to, note.token, base, fee);
    }

    function _insert(bytes32[] memory hashes) private {
        merkleRoot = keccak256(abi.encode(merkleRoot, hashes));
        rootHistory[0][merkleRoot] = true;
        nextLeafIndex += hashes.length;
    }
}
