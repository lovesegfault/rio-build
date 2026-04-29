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
# `buildScheduler.enabled=true` renders a working second-scheduler
# Deployment — the `forecast-provisioning` subtest asserts builder
# pods get `schedulerName: kube-build-scheduler` per `r[ctrl.nodeclaim.
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
  # kube-build-scheduler Deployment (templates/kube-build-scheduler.yaml)
  # `image:` is set via `buildScheduler.image` in extraValues by the
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
  # instance. `cover::build_nodeclaim` sets `nodeClassRef.{group=
  # karpenter.k8s.aws, kind=EC2NodeClass}` with name from the
  # hw-class's `node_class`; the upstream NodeClaim
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

  # Upstream KWOK fast Node/Pod stages. With `--enable-crds=Stage`
  # KWOK reads Stage CRs from the apiserver and does NOT load its
  # compiled-in defaults (cmd/root.go only seeds defaults when the
  # Stage CRD watch is off). The forecast-provisioning scenario
  # creates a fake Node from each NodeClaim; without these stages KWOK
  # would never set `Ready=True` on it nor `phase=Running` on pods
  # bound there, so `wait_worker_pod` would hang.
  kwokDefaultStages = pkgs.runCommand "kwok-default-stages.yaml" { } ''
    for f in ${
      pkgs.fetchurl {
        url = "https://raw.githubusercontent.com/kubernetes-sigs/kwok/v0.7.0/kustomize/stage/node/fast/node-initialize.yaml";
        sha256 = "1jy517kszipq4m0q6s878zlm3v7bbizdsmq146lw3vjii1pjpg13";
      }
    } ${
      pkgs.fetchurl {
        url = "https://raw.githubusercontent.com/kubernetes-sigs/kwok/v0.7.0/kustomize/stage/pod/fast/pod-ready.yaml";
        sha256 = "15sr1hlyfw6r64w3h4zbdiy5khx7c05wjgnnnkxzf0v4p64x0pg6";
      }
    } ${
      pkgs.fetchurl {
        url = "https://raw.githubusercontent.com/kubernetes-sigs/kwok/v0.7.0/kustomize/stage/pod/fast/pod-complete.yaml";
        sha256 = "1cikhnkf7r3r3rzn3b68rz7wyhr6g32dzc23iiyvvaqrfich1f46";
      }
    } ${
      pkgs.fetchurl {
        url = "https://raw.githubusercontent.com/kubernetes-sigs/kwok/v0.7.0/kustomize/stage/pod/fast/pod-delete.yaml";
        sha256 = "0wvi5pyxr8ngvgb6jhmgmvwpypk66d7p40sx8pswabbsaimdwpjs";
      }
    }; do
      cat "$f"; printf '\n---\n'
    done > $out
  '';

  # KWOK in-cluster Deployment + RBAC + Stage CRD + the Stage rules
  # that fake Karpenter. `--manage-nodes-with-annotation-selector=
  # kwok.x-k8s.io/node=fake` scopes KWOK's fake-kubelet to ONLY the
  # synthetic Nodes the test scenario creates from each Registered
  # NodeClaim — the real k3s-agent (no annotation) is left alone.
  # `--enable-crds=Stage` opts into the Stage CRD watch; KWOK's
  # `startStageController` falls through to the generic dynamic-client
  # path for any `resourceRef.kind` other than Node/Pod, so the
  # NodeClaim stages run via an unstructured informer.
  #
  # Stage chain on `karpenter.sh/v1` NodeClaim. Two non-obvious bits:
  #   - `resourceRef.apiGroup` is fed to `schema.ParseGroupVersion` by
  #     KWOK's `initStageController`; without a `/` the WHOLE string
  #     becomes the version (Group=""), so the value MUST be
  #     `karpenter.sh/v1` despite the field name.
  #   - selector keys are gojq (itchyny/gojq via `MatchExpression.key`);
  #     the array filter is `.[] | select(.type == "X")`, NOT
  #     `.[type="X"]`.
  #   nodeclaim-launched   2s: no Launched cond → Launched=True +
  #     `status.{capacity,allocatable}` = `spec.resources.requests`
  #     (so `LiveNode::from` reads the cores/mem the controller asked
  #     for) + `status.providerID` so the claim looks Karpenter-real.
  #   nodeclaim-registered 3s after: Launched=True ∧ no Registered →
  #     Registered=True + `status.nodeName=kwok-<nc>`. 5s−0s ≈
  #     boot_secs → DDSketch records ~5, lead_time gauge ~5 once one
  #     claim cycles.
  #   nodeclaim-initialized 2s after: Registered=True → Initialized=
  #     True. `LiveNode` doesn't read it but `kubectl get nodeclaim`'s
  #     READY column does (operator-debugging realism).
  #
  # Stage CANNOT create a Node (`StageNext` only patches/deletes the
  # target); the test scenario kubectl-applies a fake Node carrying the
  # NodeClaim's `metadata.labels` after observing Registered=True.
  #
  # k3s's deploy-controller uses sigs.k8s.io/yaml — strict on flow-map
  # values containing `[` / `"`. Block-style throughout; the gojq
  # selector keys are single-quoted so the embedded `|` and `"` parse
  # as scalar.
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
        resources: ["nodes", "nodes/status", "pods", "pods/status", "events"]
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
        apiGroup: karpenter.sh/v1
        kind: NodeClaim
      selector:
        matchExpressions:
          - key: '.status.conditions.[] | select(.type == "Launched")'
            operator: DoesNotExist
      delay:
        durationMilliseconds: 2000
      next:
        statusTemplate: |
          providerID: 'kwok://kwok/kwok-{{ .metadata.name }}'
          capacity:
            cpu: '{{ index .spec.resources.requests "cpu" }}'
            memory: '{{ index .spec.resources.requests "memory" }}'
            ephemeral-storage: '{{ index .spec.resources.requests "ephemeral-storage" }}'
            pods: '110'
          allocatable:
            cpu: '{{ index .spec.resources.requests "cpu" }}'
            memory: '{{ index .spec.resources.requests "memory" }}'
            ephemeral-storage: '{{ index .spec.resources.requests "ephemeral-storage" }}'
            pods: '110'
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
        apiGroup: karpenter.sh/v1
        kind: NodeClaim
      selector:
        matchExpressions:
          - key: '.status.conditions.[] | select(.type == "Launched") | .status'
            operator: In
            values: ["True"]
          - key: '.status.conditions.[] | select(.type == "Registered")'
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
    ---
    apiVersion: kwok.x-k8s.io/v1alpha1
    kind: Stage
    metadata:
      name: nodeclaim-initialized
    spec:
      resourceRef:
        apiGroup: karpenter.sh/v1
        kind: NodeClaim
      selector:
        matchExpressions:
          - key: '.status.conditions.[] | select(.type == "Registered") | .status'
            operator: In
            values: ["True"]
          - key: '.status.conditions.[] | select(.type == "Initialized")'
            operator: DoesNotExist
      delay:
        durationMilliseconds: 2000
      next:
        statusTemplate: |
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
              lastTransitionTime: '{{ (index .status.conditions 1).lastTransitionTime }}'
            - type: Initialized
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

  # Tag the chart's `buildScheduler.image` should be set to. Exposed
  # so default.nix can wire `extraValues."buildScheduler.image"`
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
    "00e-kwok-default-stages".source = kwokDefaultStages;
  };
}
