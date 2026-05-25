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
        function arbiterRule(uint256 claimId, uint256 slashAmount, bool upheld, bytes32 builder, bytes32 traceCid) external;
        function getLeader(address leader) external view returns (address addr, uint8 tier, uint256 bondAmount, uint256 claimedAum, bool active, uint64 registeredAt, uint64 bondLockedUntil);
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
        function withdraw(bytes32 paymentId) external;
    }

    #[sol(rpc)]
    contract Erc20 {
        function approve(address spender, uint256 value) external returns (bool);
        function balanceOf(address account) external view returns (uint256);
    }
}
