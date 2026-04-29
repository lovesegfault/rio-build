# KWOK + fake-Karpenter overlay for k3s-full.
#
# Validates the §13b nodeclaim_pool reconciler end-to-end without EC2.
# The upstream `karpenter-kwok` controller image (kubernetes-sigs/
# karpenter/cmd/controller -tags kwok) isn't published to a public
# registry, so this fixture plays Karpenter via KWOK Stage rules
# instead: when rio-controller creates a `karpenter.sh/v1` NodeClaim,
# KWOK's Stage CRD progresses its status (Launched=True →
# Registered=True + populate `status.capacity` from
# `spec.resources.requests`) on a 2s/5s delay. That's the subset of
# Karpenter behavior the reconciler observes: `LiveNode::from` reads
# `status.allocatable`/`conditions`, `boot_secs()` records into the
# DDSketch, `placeable` publishes.
#
# `kube-scheduler` (matching the `pins.nix` minor) is preloaded so
# `packedScheduler.enabled=true` renders a working second-scheduler
# Deployment — the `forecast-provisioning` subtest asserts builder
# pods get `schedulerName: rio-packed` per `r[ctrl.nodeclaim.
# priority-bucket]`.
{ pkgs }:
let
  pins = import ../../pins.nix;

  # KWOK controller (NOT kwokctl — that's the all-in-one CLI; we want
  # the in-cluster Deployment that watches Stage CRs). v0.7.0 is the
  # latest published tag at registry.k8s.io/kwok/kwok as of 2026-04.
  kwokImage = pkgs.dockerTools.pullImage {
    imageName = "registry.k8s.io/kwok/kwok";
    imageDigest = "sha256:2bb52d4cdd8b3e22e53ec86643a02ee84abdd8cec825269acdf7706d54c0ad6e";
    finalImageName = "registry.k8s.io/kwok/kwok";
    finalImageTag = "v0.7.0";
    hash = "sha256-3hdm2WF/AOchtCwvzRx/AeVF3fQ0JW4LcaZRZ7OnZJc=";
    os = "linux";
    arch = "amd64";
  };

  # Upstream kube-scheduler matching `pins.kubernetes_version`. The
  # rio-packed Deployment (templates/kube-scheduler-packed.yaml)
  # `image:` is set via `packedScheduler.image` in extraValues by the
  # consumer. v1.35.x latest patch — k3s-full pins the SAME minor so
  # API surface matches.
  kubeSchedulerImage = pkgs.dockerTools.pullImage {
    imageName = "registry.k8s.io/kube-scheduler";
    imageDigest = "sha256:9054fecb4fa04cc63aec47b0913c8deb3487d414190cd15211f864cfe0d0b4d6";
    finalImageName = "registry.k8s.io/kube-scheduler";
    finalImageTag = "v1.35.4";
    hash = "sha256-ekS+bXcN9z4+dS3ykpZuMaTuwErKoRnLoE+30KHG7cs=";
    os = "linux";
    arch = "amd64";
  };

  # Upstream Karpenter NodeClaim/NodePool CRDs at the pinned version.
  # rio-controller creates `karpenter.sh/v1` NodeClaims directly; the
  # CRD must be installed (k3s-full's `karpenter.enabled=false` skips
  # the chart's own EC2NodeClass/NodePool render, and the CRDs come
  # from terraform's `helm_release.karpenter` in EKS — neither runs
  # here). NOT in `nix/docker-pulled.nix`: those are images; these are
  # plain YAML FODs.
  karpenterCRDs = pkgs.runCommand "karpenter-crds.yaml" { } ''
    cat ${
      pkgs.fetchurl {
        url = "https://raw.githubusercontent.com/kubernetes-sigs/karpenter/v${pins.karpenter_version}/pkg/apis/crds/karpenter.sh_nodeclaims.yaml";
        sha256 = "0kb4z10l83fn0rj2drydzsciq6yck0a42g2llm5qlrxz0bhnn525";
      }
    } ${
      pkgs.fetchurl {
        url = "https://raw.githubusercontent.com/kubernetes-sigs/karpenter/v${pins.karpenter_version}/pkg/apis/crds/karpenter.sh_nodepools.yaml";
        sha256 = "0nxhxk65wia17v85mcsni1djz38d5303brkkx0wbrzjyapi4q7lf";
      }
    } > $out
  '';

  # Stub `karpenter.k8s.aws/v1` EC2NodeClass CRD + a `rio-default`
  # instance. `cover::build_nodeclaim` hardcodes `nodeClassRef.{group=
  # karpenter.k8s.aws, kind=EC2NodeClass}`; the upstream NodeClaim
  # CRD's openAPIV3Schema marks all three nodeClassRef fields required
  # but does NOT cross-validate existence — so a never-reconciled stub
  # CRD + empty-spec CR satisfies admission. preserveUnknownFields via
  # `x-kubernetes-preserve-unknown-fields` so any field shape passes.
  ec2NodeClassStub = pkgs.writeText "ec2nodeclass-stub.yaml" ''
    apiVersion: apiextensions.k8s.io/v1
    kind: CustomResourceDefinition
    metadata:
      name: ec2nodeclasses.karpenter.k8s.aws
    spec:
      group: karpenter.k8s.aws
      scope: Cluster
      names:
        kind: EC2NodeClass
        plural: ec2nodeclasses
        singular: ec2nodeclass
      versions:
        - name: v1
          served: true
          storage: true
          schema:
            openAPIV3Schema:
              type: object
              x-kubernetes-preserve-unknown-fields: true
    ---
    apiVersion: karpenter.k8s.aws/v1
    kind: EC2NodeClass
    metadata:
      name: rio-default
    spec: {}
    ---
    # rio-nodeclaim-shim NodePool. With `karpenter.enabled=false` the
    # chart's templates/karpenter.yaml (gated on `.Values.karpenter.
    # enabled`) doesn't render it, but `cover::build_nodeclaim` stamps
    # `karpenter.sh/nodepool: rio-nodeclaim-shim` and the NodeClaim
    # CRD's CEL doesn't require the referenced pool to exist. Installed
    # for `kubectl get nodepool` observability + so the shim-nodepool
    # spec marker has an artifact to verify against.
    apiVersion: karpenter.sh/v1
    kind: NodePool
    metadata:
      name: rio-nodeclaim-shim
    spec:
      limits:
        cpu: "0"
      template:
        spec:
          nodeClassRef:
            group: karpenter.k8s.aws
            kind: EC2NodeClass
            name: rio-default
          requirements: []
  '';

  # KWOK in-cluster Deployment + RBAC + Stage CRD + the two Stage rules
  # that fake Karpenter. KWOK's `--manage-nodes-with-annotation-
  # selector=` (empty) means it manages ZERO Nodes — we don't want fake
  # kubelets here (the k3s-agent is real); we only want the Stage
  # engine driving NodeClaim status. `--enable-crds=Stage` opts into
  # the Stage CRD watch.
  #
  # Stage `nodeclaim-launched`: at age≥2s, no Launched cond yet → set
  # `status.{capacity,allocatable}` = `spec.resources.requests` (so
  # `LiveNode::from` reads the cores/mem/disk the controller asked
  # for) + Launched=True. Stage `nodeclaim-registered`: at age≥5s,
  # Launched=True ∧ no Registered cond → Registered=True +
  # `status.nodeName` (synthetic; no real Node — `boot_secs()` only
  # reads the condition's lastTransitionTime). 5s−0s = boot_secs ≈ 5s
  # → DDSketch records ~5, lead_time gauge ~5 once one claim cycles.
  # k3s's deploy-controller uses sigs.k8s.io/yaml — strict on flow-map
  # values containing `[` / `"` (they start a sequence / quoted scalar
  # mid-flow). Block-style throughout, with the JSONPath-ish Stage
  # selector keys explicitly single-quoted.
  kwokManifests = pkgs.writeText "kwok-deploy.yaml" ''
    apiVersion: apiextensions.k8s.io/v1
    kind: CustomResourceDefinition
    metadata:
      name: stages.kwok.x-k8s.io
    spec:
      group: kwok.x-k8s.io
      scope: Cluster
      names:
        kind: Stage
        plural: stages
        singular: stage
      versions:
        - name: v1alpha1
          served: true
          storage: true
          schema:
            openAPIV3Schema:
              type: object
              x-kubernetes-preserve-unknown-fields: true
    ---
    apiVersion: v1
    kind: ServiceAccount
    metadata:
      name: kwok-controller
      namespace: kube-system
    ---
    apiVersion: rbac.authorization.k8s.io/v1
    kind: ClusterRole
    metadata:
      name: kwok-controller
    rules:
      - apiGroups: ["kwok.x-k8s.io"]
        resources: ["stages"]
        verbs: ["get", "list", "watch"]
      - apiGroups: ["karpenter.sh"]
        resources: ["nodeclaims", "nodeclaims/status"]
        verbs: ["get", "list", "watch", "update", "patch"]
      - apiGroups: [""]
        resources: ["nodes", "nodes/status", "pods", "pods/status"]
        verbs: ["get", "list", "watch", "update", "patch", "create", "delete"]
      - apiGroups: ["coordination.k8s.io"]
        resources: ["leases"]
        verbs: ["get", "list", "watch", "create", "update", "patch"]
    ---
    apiVersion: rbac.authorization.k8s.io/v1
    kind: ClusterRoleBinding
    metadata:
      name: kwok-controller
    roleRef:
      apiGroup: rbac.authorization.k8s.io
      kind: ClusterRole
      name: kwok-controller
    subjects:
      - kind: ServiceAccount
        name: kwok-controller
        namespace: kube-system
    ---
    apiVersion: apps/v1
    kind: Deployment
    metadata:
      name: kwok-controller
      namespace: kube-system
    spec:
      replicas: 1
      selector:
        matchLabels:
          app: kwok-controller
      template:
        metadata:
          labels:
            app: kwok-controller
        spec:
          serviceAccountName: kwok-controller
          containers:
            - name: kwok
              image: registry.k8s.io/kwok/kwok:v0.7.0
              args:
                - --manage-all-nodes=false
                - --manage-nodes-with-annotation-selector=kwok.x-k8s.io/node=fake
                - --enable-crds=Stage
                - --node-lease-duration-seconds=40
    ---
    apiVersion: kwok.x-k8s.io/v1alpha1
    kind: Stage
    metadata:
      name: nodeclaim-launched
    spec:
      resourceRef:
        apiGroup: karpenter.sh
        kind: NodeClaim
        version: v1
      selector:
        matchExpressions:
          - key: '.status.conditions.[type="Launched"]'
            operator: DoesNotExist
      delay:
        durationMilliseconds: 2000
      next:
        statusTemplate: |
          capacity:
            cpu: '{{ index .spec.resources.requests "cpu" }}'
            memory: '{{ index .spec.resources.requests "memory" }}'
            ephemeral-storage: '{{ index .spec.resources.requests "ephemeral-storage" }}'
          allocatable:
            cpu: '{{ index .spec.resources.requests "cpu" }}'
            memory: '{{ index .spec.resources.requests "memory" }}'
            ephemeral-storage: '{{ index .spec.resources.requests "ephemeral-storage" }}'
          conditions:
            - type: Launched
              status: "True"
              reason: KWOK
              message: ""
              lastTransitionTime: '{{ Now }}'
    ---
    apiVersion: kwok.x-k8s.io/v1alpha1
    kind: Stage
    metadata:
      name: nodeclaim-registered
    spec:
      resourceRef:
        apiGroup: karpenter.sh
        kind: NodeClaim
        version: v1
      selector:
        matchExpressions:
          - key: '.status.conditions.[type="Launched"].status'
            operator: In
            values: ["True"]
          - key: '.status.conditions.[type="Registered"]'
            operator: DoesNotExist
      delay:
        durationMilliseconds: 3000
      next:
        statusTemplate: |
          nodeName: 'kwok-{{ .metadata.name }}'
          conditions:
            - type: Launched
              status: "True"
              reason: KWOK
              message: ""
              lastTransitionTime: '{{ (index .status.conditions 0).lastTransitionTime }}'
            - type: Registered
              status: "True"
              reason: KWOK
              message: ""
              lastTransitionTime: '{{ Now }}'
  '';
in
{
  airgapImages = [
    kwokImage
    kubeSchedulerImage
  ];

  # Tag the chart's `packedScheduler.image` should be set to. Exposed
  # so default.nix can wire `extraValues."packedScheduler.image"`
  # without hardcoding the tag in two places.
  kubeSchedulerRef = "registry.k8s.io/kube-scheduler:v1.35.4";

  # k3s `services.k3s.manifests` entries. `00b-` prefix sorts after
  # `00-rio-crds` (so the NodeClaim CRD lands alongside the rio CRDs
  # before workloads that reference it) but before `01-rio-rbac` (the
  # ec2NodeClassStub `kind: EC2NodeClass` CR needs its CRD established
  # first — k3s's deploy controller retries on NotFound, but ordering
  # avoids the retry-backoff delay).
  manifests = {
    "00b-karpenter-crds".source = karpenterCRDs;
    "00c-ec2nodeclass-stub".source = ec2NodeClassStub;
    "00d-kwok".source = kwokManifests;
  };
}
