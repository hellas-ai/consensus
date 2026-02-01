{
  self,
  pkgs,
}: let
  nodePackage = self.packages.${pkgs.stdenv.hostPlatform.system}.node;
  hellasCli = self.packages.${pkgs.stdenv.hostPlatform.system}.cli;

  # Generate all crypto material deterministically from the node binary
  fixtures = builtins.fromJSON (builtins.readFile (pkgs.runCommand "hellas-test-fixtures" {
    nativeBuildInputs = [nodePackage];
  } ''
    node generate-configs --json > $out
  ''));

  nodeCount = fixtures.node_count;
  faultyCount = fixtures.faulty_count;
  p2pPort = 9000;
  grpcPort = 50051;

  blsPeers = map (p: p.bls_pubkey) fixtures.peers;
  genesisAccounts = fixtures.genesis_accounts;
  genesisAddr = (builtins.elemAt genesisAccounts 0).public_key;
  genesisBalance = toString (builtins.elemAt genesisAccounts 0).balance;

  # NixOS test assigns IPs alphabetically by node name: node0 -> .1, node1 -> .2, etc.
  nodeIp = i: "192.168.1.${toString (i + 1)}";
  nodeNames = builtins.genList (i: "node${toString i}") nodeCount;

  allValidators = builtins.genList (i: let
    peer = builtins.elemAt fixtures.peers i;
  in {
    ed25519_public_key = peer.ed25519_public_key;
    bls_peer_id = peer.bls_peer_id;
    address = "${nodeIp i}:${toString p2pPort}";
  }) nodeCount;

  makeSettings = nodeIdx: {
    consensus = {
      n = nodeCount;
      f = faultyCount;
      view_timeout = { secs = 5; nanos = 0; };
      leader_manager = "round_robin";
      network = "local";
      peers = blsPeers;
      genesis_accounts = genesisAccounts;
    };

    storage.path = "./data/node${toString nodeIdx}.redb";

    p2p = {
      listen_addr = "0.0.0.0:${toString p2pPort}";
      external_addr = "${nodeIp nodeIdx}:${toString p2pPort}";
      total_number_peers = nodeCount;
      maximum_number_faulty_peers = faultyCount;
      bootstrap_timeout_ms = 30000;
      ping_interval_ms = 200;
      cluster_id = "hellas-local";
      validators = builtins.filter
        (v: v.address != "${nodeIp nodeIdx}:${toString p2pPort}")
        allValidators;
    };

    rpc = {
      listen_addr = "0.0.0.0:${toString grpcPort}";
      peer_id = nodeIdx;
      max_concurrent_streams = 100;
      request_timeout_secs = 30;
      network = "local";
      total_validators = nodeCount;
      f = faultyCount;
    };

    identity = {};
  };

  makeNode = nodeIdx: {... }: {
    imports = [self.nixosModules.hellas];

    services.hellas.validators.node = {
      enable = true;
      openFirewall = true;
      inherit p2pPort grpcPort;
      settings = makeSettings nodeIdx;
    };

    environment.systemPackages = [hellasCli];
  };
in
  pkgs.testers.nixosTest {
    name = "hellas-consensus";

    nodes = builtins.listToAttrs (builtins.genList (i: {
      name = "node${toString i}";
      value = makeNode i;
    }) nodeCount);

    testScript = ''
      nodes = [${builtins.concatStringsSep ", " nodeNames}]

      start_all()

      for m in nodes:
          m.wait_for_unit("hellas-validator-node.service")

      # Wait for consensus bootstrap — poll until CLI can query blocks
      for m in nodes:
          m.wait_until_succeeds("hellas block latest", timeout=60)

      # Verify P2P mesh connectivity
      for i in range(${toString nodeCount}):
          for j in range(${toString nodeCount}):
              if i != j:
                  nodes[i].succeed(f"nc -z 192.168.1.{j + 1} ${toString p2pPort}")

      # Genesis account: full info, bare balance, nonce
      output = node0.succeed("hellas account get ${genesisAddr}")
      assert "balance: ${genesisBalance}" in output, f"Unexpected account output: {output}"

      balance = node0.succeed("hellas account balance ${genesisAddr}").strip()
      assert balance == "${genesisBalance}", f"Expected balance ${genesisBalance}, got {balance}"

      nonce = node0.succeed("hellas account nonce ${genesisAddr}").strip()
      assert nonce == "0", f"Expected nonce 0, got {nonce}"

      # Block queries
      output = node0.succeed("hellas block height 0")
      assert "height:       0" in output, f"Genesis block query failed: {output}"

      # Non-existent account should error
      node0.fail("hellas account get 0000000000000000000000000000000000000000000000000000000000000000")

      # Cross-node query via --endpoint
      output = node0.succeed("hellas -e http://192.168.1.2:${toString grpcPort} block latest")
      assert "height:" in output, f"Cross-node query failed: {output}"
    '';
  }
