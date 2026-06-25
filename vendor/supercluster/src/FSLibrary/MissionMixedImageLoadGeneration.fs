// Copyright 2022 Stellar Development Foundation and contributors. Licensed
// under the Apache License, Version 2.0. See the COPYING file at the root
// of this distribution or at http://www.apache.org/licenses/LICENSE-2.0

module MissionMixedImageLoadGeneration

open stellar_dotnet_sdk
open StellarCoreSet
open StellarMissionContext
open StellarFormation
open StellarStatefulSets
open StellarSupercluster
open StellarCoreHTTP
open StellarCorePeer

let protocolSupported (formation: StellarFormation) (coreSets: list<CoreSet>) : bool =
    // UpgradeProtocolToLatest upgrades to the version supported by the first peer in coreSets first coreSet
    let supportedProtocol = (formation.NetworkCfg.GetPeer coreSets.[0] 0).GetSupportedProtocolVersion()

    coreSets
    |> List.forall
        (fun coreSet ->
            let peer = formation.NetworkCfg.GetPeer coreSet 0
            peer.GetSupportedProtocolVersion() >= supportedProtocol)

let mixedImageLoadGeneration (oldImageNodeCount: int) (context: MissionContext) =
    let oldNodeCount = oldImageNodeCount
    let newNodeCount = 3 - oldImageNodeCount

    let newImage = context.image
    let oldImage = GetOrDefault context.oldImage newImage

    let oldName = "core-old"
    let newName = "core-new"

    // Allow 2/3 nodes consensus, so that one image version could fork the network
    // in case of bugs.
    // Only reference non-empty core sets in the quorum set (supports the
    // all-old / all-new compositions where one set has zero nodes).
    let activeNames =
        [ if oldNodeCount > 0 then yield CoreSetName oldName
          if newNodeCount > 0 then yield CoreSetName newName ]

    let qSet =
        CoreSetQuorumListWithThreshold((activeNames, 51))

    let oldCoreSet =
        MakeLiveCoreSet
            oldName
            { CoreSetOptions.GetDefault oldImage with
                  nodeCount = oldNodeCount
                  invariantChecks = AllInvariantsExceptEvents
                  accelerateTime = false
                  dumpDatabase = false
                  quorumSet = qSet
                  // FIXME: Remove these options once the stable (old) image in
                  // CI supports skipping validator quality checks
                  skipHighCriticalValidatorChecks = false
                  quorumSetConfigType = RequireExplicitQset }

    let newCoreSet =
        MakeLiveCoreSet
            newName
            { CoreSetOptions.GetDefault newImage with
                  nodeCount = newNodeCount
                  invariantChecks = AllInvariantsExceptEvents
                  accelerateTime = false
                  dumpDatabase = false
                  quorumSet = qSet }

    let context =
        { context.WithSmallLoadgenOptions with
              // SmallTestResources is tuned in this vendored fork (512MB req /
              // 2.5GB limit) so the henyey image is not OOM-killed under the
              // heavy mixed-image profile on nsc 8GB nodes. See StellarKubeSpecs
              // SmallTestCoreResourceRequirements and VENDOR.md.
              coreResources = SmallTestResources
              numAccounts = context.numAccounts
              numTxs = context.numTxs
              txRate = context.txRate
              skipLowFeeTxs = true
              genesisTestAccountCount = context.genesisTestAccountCount }

    // Put the version with majority of nodes in front of the set to let it generate
    // the load and possibly leave the minority of nodes out of consensus in case of bugs.
    // Majority image first; drop any empty (zero-node) set so the all-old /
    // all-new compositions become clean single-image networks.
    let coreSets =
        match oldNodeCount, newNodeCount with
        | _, 0 -> [ oldCoreSet ]
        | 0, _ -> [ newCoreSet ]
        | o, n when o > n -> [ oldCoreSet; newCoreSet ]
        | _ -> [ newCoreSet; oldCoreSet ]

    context.Execute
        coreSets
        None
        (fun (formation: StellarFormation) ->
            formation.WaitUntilSynced coreSets

            // End mission early if we can't upgrade all nodes
            if protocolSupported formation coreSets then
                formation.UpgradeProtocolToLatest coreSets
                formation.UpgradeMaxTxSetSize coreSets 1000

                let loadgenCoreSet = coreSets.[0]
                formation.RunLoadgen loadgenCoreSet context.GeneratePaymentLoad

                let majorityPeer = formation.NetworkCfg.GetPeer loadgenCoreSet 0

                if majorityPeer.GetLedgerProtocolVersion() >= 20 then
                    formation.UpgradeSorobanLedgerLimitsWithMultiplier coreSets 100
                    formation.RunLoadgen loadgenCoreSet { context.GenerateSorobanUploadLoad with txrate = 1; txs = 200 })

let mixedImageLoadGenerationWithOldImageMajority (context: MissionContext) = mixedImageLoadGeneration 2 context

let mixedImageLoadGenerationWithNewImageMajority (context: MissionContext) = mixedImageLoadGeneration 1 context

// All-old-image (3 core, 0 henyey) — pure-core baseline.
let mixedImageLoadGenerationAllOldImage (context: MissionContext) = mixedImageLoadGeneration 3 context

// All-new-image (0 core, 3 henyey) — pure-henyey.
let mixedImageLoadGenerationAllNewImage (context: MissionContext) = mixedImageLoadGeneration 0 context
