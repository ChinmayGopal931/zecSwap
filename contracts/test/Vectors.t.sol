// SPDX-License-Identifier: Apache-2.0
pragma solidity 0.8.28;

import {IRailgun} from "../src/ShieldVault.sol";
import {ZecSwap} from "../src/ZecSwap.sol";
import {PallasHarness} from "./utils/PallasHarness.sol";
import {SpendAuthVectors} from "./utils/SpendAuthVectors.sol";

contract VectorsTest is SpendAuthVectors {
    bytes32 internal constant RESCUE_TYPEHASH =
        keccak256("Rescue(bytes32 id,bytes32 note,address relayer,uint128 fee,uint64 nonce,uint64 deadline)");

    PallasHarness internal pallas = new PallasHarness();
    ZecSwap internal swaps = new ZecSwap(30 minutes, 1 hours, IRailgun(address(0)));

    function test_pallasMatchesRustVectors() public view {
        assertEq(vectors.length, FIRST_RANDOM + RANDOM_COUNT);
        for (uint256 i; i < vectors.length; ++i) {
            Vector memory v = vectors[i];
            assertTrue(pallas.isOnCurve(v.x, v.y), "vector point on curve");
            assertTrue(pallas.isSpendAuthMul(v.k, v.x, v.y), "vector matches");
        }
    }

    function test_hashTermsMatchesTheWalletVectors() public view {
        address vectorMaker = 0x09eD1F966745Be18C711C346242c0974DAd7c3e5;
        address token = 0x3333333333333333333333333333333333333333;
        address swapKey = 0x757De38c2d9880E44AB59827D1622403fBF88Ff5;
        uint256[2] memory makerKey = [
            0x187d300ebb59a5c9e7c9e61debd0b535a8a5cbbad4a5c46ac2a0597a06c13ee4,
            0x329a369cf900745cdca87863f3f00de64d16e088bb1f5da341a2977547f9080a
        ];
        uint256[2] memory userKey = [
            0x0b42629d5b3f787aba7ccde87574c13f6db6b8fccb7c5cfeb3f4e4081c756461,
            0x311662156525fcaa692aeef5d2362c2a9ab2083cd6d968c618aec72617aa925a
        ];
        ZecSwap.Terms memory paysAccount = ZecSwap.Terms(
            vectorMaker,
            token,
            150_000_000,
            makerKey,
            userKey,
            0x4444444444444444444444444444444444444444,
            1_790_003_600,
            1_790_007_200,
            bytes32(0)
        );
        assertEq(
            swaps.hashTerms(paysAccount), 0x909f60119225ae64356011c63945694ed0dc6aaaa8cd6002a897df9ab76efc66
        );
        paysAccount.user = swapKey;
        paysAccount.payoutNote = 0x5af6901ba7cb01f49785a29c4a2e57e31af3e53382ce3dd2e35678897515ffc1;
        assertEq(
            swaps.hashTerms(paysAccount), 0xb7ca9333e9e443218d19c3b8aa345fa67a671ac24060877de1498efb01c99a29
        );
        ZecSwap.Terms memory reverse = ZecSwap.Terms(
            swapKey,
            token,
            50_000_000,
            userKey,
            makerKey,
            vectorMaker,
            1_790_003_600,
            1_790_007_200,
            bytes32(0)
        );
        assertEq(swaps.hashTerms(reverse), 0xa295e46f6db15997af4a950b5e6deed6ae5d3161b6571b2f445a34bc6570cead);
    }

    function test_rescueTypedDataMatchesTheNativeSignerVector() public pure {
        bytes32 domain = keccak256(
            abi.encode(
                keccak256(
                    "EIP712Domain(string name,string version,uint256 chainId,address verifyingContract)"
                ),
                keccak256("ZecSwap"),
                keccak256("1"),
                uint256(11155111),
                address(0x1111111111111111111111111111111111111111)
            )
        );
        bytes32 typed = keccak256(
            abi.encode(
                RESCUE_TYPEHASH,
                bytes32(0xf222c5c748f566811318f3e2851848301cf248bb706b32a98278936350465ed7),
                bytes32(0x5af6901ba7cb01f49785a29c4a2e57e31af3e53382ce3dd2e35678897515ffc1),
                address(0x2222222222222222222222222222222222222222),
                uint128(20000),
                uint64(0),
                uint64(1790000000)
            )
        );
        address recovered = ecrecover(
            keccak256(abi.encodePacked("\x19\x01", domain, typed)),
            27,
            0x91eab39afaafa2c37bfd18b4b64436386a8e2d19bb04e866f5178cc8f1878894,
            0x4999e9a2c9fabac3cda546d0fd22ad9faa53ca75759637ab6a42975f33278bea
        );
        assertEq(recovered, address(0x757De38c2d9880E44AB59827D1622403fBF88Ff5));
    }
}
