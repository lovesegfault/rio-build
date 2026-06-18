# Aurora PostgreSQL Serverless v2 for rio-scheduler + rio-store.
#
# Both services share one database. Migrations namespace via table
# names (scheduler tables: builds, derivations, assignments, tenants;
# store tables: narinfo, chunks, manifests). sqlx's advisory-lock
# migration mechanism (`_sqlx_migrations` table) handles concurrent
# migration attempts from the two services racing at startup —
# first one wins, second one sees its migrations already applied.
#
# Connection string (templated by ESO in the rio-postgres ExternalSecret):
#   postgres://rio:<pw>@<endpoint>:5432/rio?sslmode=verify-full&sslrootcert=<bundle>
#
# Aurora PG 15+ has rds.force_ssl=1 by default; sqlx's
# tls-rustls-aws-lc-rs feature handles the TLS handshake.
# sslmode=verify-full verifies the server cert against the vendored
# RDS trust bundle the chart mounts into every PG-consumer pod
# (infra/helm/rio-build/files/rds-global-bundle.pem). Verification is
# mandatory for IAM auth — the 15-minute token is a replayable bearer
# credential and must only be sent to a verified server — and a strict
# win for password mode too.

# Subnet group: dual-stack database tier (NOT private_subnets — those
# are ipv6_native and AWS rejects a DB subnet group whose subnets lack
# a v4 CIDR, even with network_type=DUAL on the cluster). rio-store
# connects via the cluster endpoint's AAAA from the v6-only node tier.
resource "aws_db_subnet_group" "rio" {
  name       = "${var.cluster_name}-aurora"
  subnet_ids = module.vpc.database_subnets
}

# Security group: inbound 5432 from the EKS node SG only.
# node_security_group_id, NOT cluster_primary_security_group_id —
# the cluster SG is attached to control-plane ENIs, not worker
# nodes. Pods (via VPC CNI) inherit the NODE security group.
# Using the wrong one = silent connection timeouts (no RST, the
# SYN just drops).
resource "aws_security_group" "aurora" {
  name   = "${var.cluster_name}-aurora"
  vpc_id = module.vpc.vpc_id

  ingress {
    from_port       = 5432
    to_port         = 5432
    protocol        = "tcp"
    security_groups = [module.eks.node_security_group_id]
  }

  # No egress rule: Aurora initiates no outbound connections
  # (no logical replication, no extensions that phone home).
  # Empty egress = deny-all-outbound, which is correct.
}

# PG connection budget: tf is the model, the deploy preflight is the
# measurement. Consumers (xtask deploy, the chart's store ceiling)
# receive the derived value — they never re-derive it from prose.
#
# Source (fetched 2026-06-03): AWS Aurora User Guide, "Performance and
# scaling for Aurora Serverless v2"
# (aurora-serverless-v2.setting-capacity.html, section "Maximum
# connections for Aurora Serverless v2", anchor
# #aurora-serverless-v2.max-connections), table "default values for
# max_connections … based on the maximum ACU value", column
# "Default maximum connections on Aurora PostgreSQL":
#   1 → 189, 4 → 823, 8 → 1,669, 16 → 3,360, ≥32 → 5,000.
# Table Note (PostgreSQL): an instance with a minimum capacity of
# 0 or 0.5 ACUs is CAPPED at 2,000 connections — min_capacity
# participates in the budget ONLY through that cap.
#
# WARNING: the adjacent column in the same AWS table is Aurora MYSQL
# (16 → 2,000, 32 → 3,000). That column is the source of BOTH prior
# wrong derivations on this branch (the "~2,800" chain and the
# "32 ⇒ ~3,000" chain). Do not "correct" the PostgreSQL values back
# to it.
#
# Semantics (both load-bearing for the deploy preflight):
#   - max_connections is keyed on max_capacity (memory at MAXIMUM
#     ACUs) and held RUNTIME-CONSTANT — it does NOT scale with the
#     live ACU; low-ACU operation merely has less memory per
#     connection.
#   - it is a STATIC parameter: a capacity-range change takes effect
#     only after an instance REBOOT, so a capacity flip passes the
#     preflight's strict-equality check only post-reboot (plan the
#     reboot into the flip).
locals {
  aurora_min_capacity = 1
  aurora_max_capacity = 32

  # The FULL AWS PostgreSQL column (not just today's value) so the
  # precondition below never fires on a legitimate resize.
  aurora_pg_max_connections_by_max_acu = {
    "1"   = 189
    "4"   = 823
    "8"   = 1669
    "16"  = 3360
    "32"  = 5000
    "64"  = 5000
    "128" = 5000
    "192" = 5000
    "256" = 5000
  }

  # The table-Note cap rule: PostgreSQL with min_capacity <= 0.5 ACU
  # is capped at 2,000 regardless of max_capacity.
  expected_pg_max_connections = (
    local.aurora_min_capacity <= 0.5
    ? min(2000, local.aurora_pg_max_connections_by_max_acu[tostring(local.aurora_max_capacity)])
    : local.aurora_pg_max_connections_by_max_acu[tostring(local.aurora_max_capacity)]
  )
}

resource "aws_rds_cluster" "rio" {
  cluster_identifier = "${var.cluster_name}-pg"
  engine             = "aurora-postgresql"
  # 18.x: latest Aurora-supported major as of writing. Aurora lags
  # upstream PG by ~6 months. Check `aws rds describe-db-engine-
  # versions --engine aurora-postgresql` if this errors on apply.
  # Major bumps on a live cluster: see docs/ops/eks-smoke.typ "Aurora
  # major-version upgrade" — `terraform apply` alone is NOT enough.
  engine_version              = "18.3"
  allow_major_version_upgrade = true
  # "provisioned" + serverlessv2_scaling_configuration = Serverless v2.
  # engine_mode "serverless" is Serverless V1 (deprecated, don't use).
  engine_mode   = "provisioned"
  database_name = "rio"

  master_username = "rio"
  # manage_master_user_password: Aurora generates a password and
  # stores it in Secrets Manager. No sensitive values in tfstate.
  # ESO syncs it into the rio-postgres Secret (see secrets.tf + the
  # chart's external-secrets.yaml template). CAVEAT: AWS rotates it
  # every 7 days, and pods read the synced Secret as env frozen at
  # pod start — every rotation broke DB auth until pods restarted.
  # That incident is why IAM auth (below) exists.
  manage_master_user_password = true

  # RDS IAM database authentication: pods mint 15-minute SigV4 tokens
  # from their IRSA roles (rds-db:connect — main.tf rio_rds_connect)
  # and connect as DB user rio_app (the migrate runner's ensure_roles
  # pass creates it with rds_iam membership) — no static credential to
  # rotate out from under a running pod. NOT a free flip for
  # password-mode clients: with this flag on, RDS PAM rejects password
  # auth for any role that holds rds_iam directly OR BY INHERITANCE —
  # which is why ensure_roles never grants rio_app (a member of
  # rds_iam) to the master, and detaches any legacy membership it
  # finds. helm postgres.authMode is the client-side switch.
  iam_database_authentication_enabled = true

  db_subnet_group_name   = aws_db_subnet_group.rio.name
  vpc_security_group_ids = [aws_security_group.aurora.id]
  # DUAL: cluster endpoint gets both A and AAAA. v6-only pods (the
  # cluster_ip_family above) connect via AAAA; any v4-only tooling
  # still resolves A. Subnets must have v6 CIDRs (enable_ipv6 in the
  # vpc module).
  network_type = "DUAL"

  serverlessv2_scaling_configuration {
    # Raised 0.5 → 1 ACU (owner decision Q1, 2026-06-03): at
    # min_capacity ≤ 0.5 the AWS PG table Note caps max_connections
    # at 2,000 regardless of max_capacity — the 2026-06-02 16→32
    # max_capacity raise did NOT lift the connection budget because
    # this knob stayed at 0.5. At 1 ACU the cap no longer applies and
    # the budget is the table value at max_capacity (5,000 at 32).
    # Cost: ~2 GB RAM floor, ~$88/mo at idle (was ~$44/mo at 0.5).
    min_capacity = local.aurora_min_capacity
    # I-110: ephemeral builders' QueryPathInfo burst (~800 QPI ×
    # N builders) saturates connections. At 2 ACU (~360 max_conn),
    # 4×50 store pool conns + scheduler hit the cap → 11s acquire
    # times → builder FUSE circuit opens → builds fail.
    #
    # Connection budget: see the locals block above — the AWS PG
    # column is data (aurora_pg_max_connections_by_max_acu), the
    # min-capacity cap is the encoded rule, and the derived value
    # (expected_pg_max_connections, exported as the
    # pg_max_connections output) is what xtask deploy's pg preflight
    # asserts against the live server and what the store ceiling is
    # derived from. Change capacity HERE; consumers follow.
    max_capacity = local.aurora_max_capacity
  }

  lifecycle {
    precondition {
      condition = contains(
        keys(local.aurora_pg_max_connections_by_max_acu),
        tostring(local.aurora_max_capacity),
      )
      error_message = "aurora_max_capacity has no entry in aurora_pg_max_connections_by_max_acu (rds.tf) — add the AWS PG-column value for this ACU setting before applying."
    }
  }

  # Don't snapshot on destroy — this is dev/test. For prod, set
  # final_snapshot_identifier and remove skip_final_snapshot.
  skip_final_snapshot = true

  # Apply changes immediately (not during maintenance window). Dev
  # cluster — waiting for a maintenance window to bump max_capacity
  # is silly.
  apply_immediately = true
}

# Serverless v2 still needs at least one instance in the cluster.
# The instance does the actual serving; the cluster is metadata +
# storage. instance_class = db.serverless is the magic value that
# makes the instance use the cluster's serverlessv2 scaling config.
resource "aws_rds_cluster_instance" "rio" {
  identifier         = "${var.cluster_name}-pg-1"
  cluster_identifier = aws_rds_cluster.rio.id
  instance_class     = "db.serverless"
  engine             = aws_rds_cluster.rio.engine
  engine_version     = aws_rds_cluster.rio.engine_version
}
