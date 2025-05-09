# Network Registry

To test HOPR protocol and develop dApps on top of HOPR at a resonable scale, nodes are only allowed to join the network (sending messages) if they are registered on a "Network Registry" smart contract.

This restriction on the access guarded by the "Network Registry" is only enabled in the staging or production environment by default.  
If you are running a cluster of HOPR nodes locally in the anvil network, the "Network Registry" is not enabled.

There are two ways of registering a node:

- By the node runner itself, providing the node runner is eligible; or
- By the owner of the `HoprNetworkRegistry` smart contract

Relevant smart contracts are listed below, per environment **(to be updated)**:

| Contract                 | Production                                                                                                             |
| ------------------------ | ---------------------------------------------------------------------------------------------------------------------- |
| HoprNetworkRegistry      | [0x582b4b586168621dAf83bEb2AeADb5fb20F8d50d](https://gnosisscan.io/address/0x582b4b586168621dAf83bEb2AeADb5fb20F8d50d) |
| HoprNetworkRegistryProxy | [0x2bc6b78B0aA892e97714F0e3b1c74487f92C5884](https://gnosisscan.io/address/0x2bc6b78B0aA892e97714F0e3b1c74487f92C5884) |

## Register a node by the runner

Self registration is currently disabled.

<!-- ### Eligibility

A node can be registered by its runner if the runner is eligible. There are two ways to become an eligible account:

- A node runner's Ethereum account is staking in the HOPR stake program for a minimum stake of 1000 xHOPR token and one of the NFTs from the "allowed list"
- A node runner's Ethereum account is staking a "HOPR Boost NFT" of type `Network_registry`

#### Stake xHOPR tokens in staging environment

To stake xHOPR tokens, you can interact directly with the staking contract of the environment your HOPR node is running on. For production network, there is even a web application for such a purpose.

For the <mark>staging environment</mark>, please call the following function where the `PRIVATE_KEY` is the private key of the node runner's account. This call can only succeed if the caller (i.e. the `PRIVATE_KEY` or the node runner) has enough xHOPR (in staging environment).

```
PRIVATE_KEY=<private key of "account"> make stake-funds network=rotsee environment_type=staging
```

If there's not enough xHOPR token, please use "Dev Bank" account to transfer some to the node runner's account.

#### Stake Network_registry NFT in staging environment

<mark>When not in production</mark>, CI/CD will mint "Network_registry" NFTs to its own wallet on deployment.

There are 6 "Network_registry" NFTs (3 "developer" rank and 3 of "community" rank) being minted to the "Dev Bank" account per deployment, where you can transfer some tokens from.

For the <mark>staging environment</mark>, please call the following function where the `PRIVATE_KEY` is the private key of the node runner's account. This call can only succeed if the caller (i.e. the `PRIVATE_KEY` or the node runner) has "Network_registry" NFT (in staging environment).

```
PRIVATE_KEY<private key of "account"> make stake-nrnft network=rotsee environment_type=staging nftrank=<rank of "Network_registry" nft>
``` -->

### Register a node

An eligible node runner can call `selfRegister(address[] nodeAddresses)` method on `HoprNetworkRegistry` smart contract to register one or multiple HOPR node(s).
This function should be called from the Safe holding HOPR token assets.

## Deregister a node

A node runner can call `selfDeregister(address[] nodeAddresses)` method on `HoprNetworkRegistry` smart contract to remove previously registered HOPR nodes.
This function should be called from the Safe holding HOPR token assets.

## Register a node by the Network Registry contract owner

### Eligibility

Owner can register any account for any node. The eligibility of an account is not going to be checked unless a `sync` method for that account gets called.

### Register a node

Owner can call `managerRegister(address[] accounts, address[] nodeAddresses)` method from `HoprNetworkRegistry` smart contract to register a list of HOPR nodes for a list of accounts respectively. Note that this registration can overwrite existing entries.

```
make register-nodes network=rotsee environment_type=staging staking_addresses=<address1,address2,address3,address4> node_addresses=<nodeAddress1,nodeAddress2,nodeAddress3,nodeAddress4>
```

## Deregister a node

Owner can call `managerDeregister(address[] nodeAddresses)` method from `HoprNetworkRegistry` smart contract to remove a list of nodes.

```
make deregister-nodes network=rotsee environment_type=staging node_addresses=<nodeAddress1,nodeAddress2>
```

## Enable and disable globally

As mentioned in the beginning, by default, Network Registry is enabled for staging envirionment and disabled in the local network.
To toggle the network registry, the following method can be called

```
make disable-network-registry network=rotsee environment_type=staging
```

or

```
make enable-network-registry network=rotsee environment_type=staging
```

<!-- ## Internal NR testing

### Staging

To register an eligible node in the NR, there are two options:

- obtain a "Network_registry" NFT and register your node on NR
- stake tokens and register your node on NR

The procedure for both options are very similar, which only some differences in the last step.

#### Procedure

1. Create a MetaMask wallet (note as “account”)
2. Fund 1 Goerli ETH (from “DevBank” or from the faucet) to the “account”
3. Start your local HOPR node
4. Save private keys (`ACCOUNT_PRIVKEY` and `DEV_BANK_PRIVKEY`) into `.env` file

```
export ACCOUNT_PRIVKEY=<account_private_key>
export DEV_BANK_PRIVKEY=<dev_bank_private_key>
```

and

```
source .env
```

5. Run either command. In both cases, provide `<hoprd_endpoint>` when it's different from `localhost:3001`

- Option 1: obtain a "Network_registry" NFT (with nftrank of "developer" or "community") and register your node on NR

  ```
  make register-node-with-nft endpoint=<hoprd_endpoint> nftrank=<"Network_registry" NFT Rank> account=<staking_account> network=rotsee environment_type=staging
  ```

- Option 2: stake tokens and register your node on NR

  ```
  make register-node-with-stake endpoint=<hoprd_endpoint> account=<staking_account> network=rotsee environment_type=staging
  ```

### Production

For nodes running in the upcoming "monte_rosa" network, only wallets with one "Network_registry" HoprBoost NFT (be `developer` or `community` rank) staked in the staking program are eligible to spin up HOPR nodes.

To register one (`community` rank) or many (`developer` rank) eligible node in the NR, please follow:

#### Procedure

1. Create a MetaMask wallet (note as “account”)
2. Fund “account” with some xDAI (e.g 0.1 xDAI)
3. Start your local HOPR node
4. Save private keys (`ACCOUNT_PRIVKEY` and `DEV_BANK_PRIVKEY`) into `.env` file

   ```
   export ACCOUNT_PRIVKEY=<account_private_key>
   ```

   and

   ```
   source .env
   ```

5. Request "Network_registry" NFT. Either by requesting from TECH (or COM) team, or by transferring it directly from "Dev Bank".

6. Register nodes and eligible accounts onto Network Registry. There are three options:

   a. For the **deployer** wallet: Deployer wallet should stake one `developer` NFT and
   register some peer ids.
   To stake one `developer` NFT:

   ```
   PRIVATE_KEY=${ACCOUNT_PRIVKEY} make stake-nrnft network=monte_rosa environment_type=production \
      nftrank=developer
   ```

   To register some peers:

   1. When "staking proxy" is used:

      ```
      PRIVATE_KEY=${ACCOUNT_PRIVKEY} make self-register-node network=monte_rosa environment_type=production \
         node_addresses=<peerId1,peerId2,peerId3,peerId4...>
      ```

   2. When "dummy proxy" is used:
      ```
      PRIVATE_KEY=${ACCOUNT_PRIVKEY} make sync-eligibility network=monte_rosa environment_type=production \
         node_addresses=<peerId1,peerId2,peerId3,peerId4...>
      ```

   b. For community/team testing:

   ```
   PRIVATE_KEY=${ACCOUNT_PRIVKEY} make stake-nrnft nftrank=<"developer" or "community"> network=monte_rosa environment_type=production
   PRIVATE_KEY=${ACCOUNT_PRIVKEY} make self-register-node node_addresses=<peerId1,peerId2,peerId3,peerId4...> network=monte_rosa environment_type=production
   ``` -->
