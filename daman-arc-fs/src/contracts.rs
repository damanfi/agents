//! Alloy `sol!` bindings for the Daman + substrate contract surface. Single source of
//! truth for the ABI shapes the tool factories submit against.

use alloy::sol;

sol! {
    #[sol(rpc)]
    contract CopyBond {
        function registerLeader(uint8 tier, uint256 claimedAum) external;
        function postBond(uint256 amount) external;
        function withdrawBond(uint256 amount) external;
        function subscribe(address leader, uint256 capital, bytes32 builder) external;
        function unsubscribe(address leader) external;
        function attestDegradation(address leader, bytes32 evidenceHash, bytes32 builder) external returns (uint256);
        function disputeAttestation(uint256 claimId) external;
        function arbiterRule(uint256 claimId, uint256 slashAmount, bool upheld, bytes32 builder, bytes32 traceCid) external;

        function getLeader(address leader) external view returns (
            address addr,
            uint8 tier,
            uint256 bondAmount,
            uint256 claimedAum,
            uint64 registeredAt,
            uint64 bondLockedUntil,
            bool active
        );
        function getClaim(uint256 claimId) external view returns (
            uint256 id,
            address leader,
            address watchdog,
            bytes32 evidenceHash,
            uint64 filedAt,
            uint64 disputeWindowEnds,
            uint8 status,
            uint256 slashAmount,
            bytes32 builder
        );
        function getSubscription(address follower, address leader) external view returns (
            address follower_,
            address leader_,
            uint256 capital,
            uint64 since,
            bytes32 builder
        );
        function bondBalance(address leader) external view returns (uint256);

        event DegradationFlagged(
            uint256 indexed claimId,
            address indexed leader,
            address indexed watchdog,
            bytes32 evidenceHash,
            bytes32 builder
        );
    }

    #[sol(rpc)]
    contract BountyAccrual {
        function claimBounty(uint256 claimId) external;
        function bountyAmount(uint256 claimId) external view returns (uint256);
        function bountyRecipient(uint256 claimId) external view returns (address);
        function bountyClaimed(uint256 claimId) external view returns (bool);
    }

    #[sol(rpc)]
    contract ReputationRegistry {
        function reputationScore(address agent) external view returns (int256);
        function cumulativeUpheld(address agent) external view returns (uint256);
        function cumulativeRejected(address agent) external view returns (uint256);
    }

    /// Sibling deploy of DamanReputationRegistry used as the boot-time
    /// agent identity anchor. Same code as ReputationRegistry; different
    /// address (`addrs.agent_registry`). The `register(bytes32 role)`
    /// call is permissionless and self-stamps msg.sender with a role and
    /// initial activity timestamp. Reverts `AlreadyRegistered` on a
    /// second call from the same address; the persona binary treats
    /// that as success.
    #[sol(rpc)]
    contract AgentRegistry {
        function register(bytes32 role) external;
        function isRegistered(address agent) external view returns (bool);
        function roleOf(address agent) external view returns (bytes32);
    }

    #[sol(rpc)]
    contract Benevolence {
        struct LoanRequest {
            address borrower;
            uint256 amount;
            uint256 nonce;
            uint256 deadline;
        }
        function requestLoan(uint256 amount) external;
        function requestLoanWithSignature(LoanRequest calldata req, bytes calldata signature) external;
        function repay(uint256 amount) external;
        function debtOf(address borrower) external view returns (uint256);
        function nonceOf(address borrower) external view returns (uint256);
        function isEligible(address candidate) external view returns (bool);
        function treasuryAvailable() external view returns (uint256);
    }

    #[sol(rpc)]
    contract RefundProtocol {
        function withdraw(uint256[] calldata paymentIDs) external;
        function payments(uint256 paymentID) external view returns (
            address to,
            uint256 amount,
            uint256 releaseTimestamp,
            address refundTo,
            uint256 withdrawnAmount,
            bool refunded
        );
        function balances(address account) external view returns (uint256);
        function debts(address account) external view returns (uint256);
        function paused() external view returns (bool);
    }

    /// Curator-bounded asset whitelist consulted by DamanCopyBond.recordTrade.
    /// Bees only need the read surface; curator mutators (addAsset / removeAsset /
    /// setSource / rotateCurator / pause / unpause) are intentionally omitted
    /// because the bee EOA is not the curator.
    #[sol(rpc)]
    contract UniverseRegistry {
        function isEligible(address asset) external view returns (bool);
        function listAssets() external view returns (address[] memory);
        function sourceTag() external view returns (bytes32);
        function lastUpdatedAt() external view returns (uint64);
        function curator() external view returns (address);
        function paused() external view returns (bool);
    }

    #[sol(rpc)]
    contract Erc20 {
        function approve(address spender, uint256 value) external returns (bool);
        function balanceOf(address account) external view returns (uint256);
        function allowance(address owner, address spender) external view returns (uint256);
    }
}
