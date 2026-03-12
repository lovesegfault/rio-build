# Aurora PostgreSQL Serverless v2 for rio-scheduler + rio-store.
#
# Both services share one database. Migrations namespace via table
# names (scheduler tables: builds, derivations, assignments, tenants;
# store tables: narinfo, chunks, manifests). sqlx's advisory-lock
# migration mechanism (`_sqlx_migrations` table) handles concurrent
# migration attempts from the two services racing at startup —
# first one wins, second one sees its migrations already applied.
#
# Connection string (built in deploy.sh from outputs):
#   postgres://rio:<pw>@<endpoint>:5432/rio?sslmode=require
#
# sslmode=require because Aurora PG 15+ has rds.force_ssl=1 by
# default. sqlx's tls-rustls-aws-lc-rs feature (added in C1 of
# this work) handles the TLS handshake. `require` encrypts but
# doesn't verify the cert — fine for in-VPC + SG-restricted.
# `verify-full` would need the RDS CA bundle mounted into pods.

# Subnet group: private subnets only. Aurora in public subnets
# is almost always wrong (it gets a public IP if publicly_accessible
# is set, which we don't, but the subnet choice is still hygiene).
resource "aws_db_subnet_group" "rio" {
  name       = "${var.cluster_name}-aurora"
  subnet_ids = module.vpc.private_subnets
}

# Security group: inbound 5432 from the EKS cluster SG only.
# The EKS module creates a "cluster security group" attached to all
# managed-node-group nodes; pods (via VPC CNI) share that SG. So
# "from the cluster SG" = "from any pod in the cluster", which is
# what we want — scheduler and store pods can connect, nothing else.
resource "aws_security_group" "aurora" {
  name   = "${var.cluster_name}-aurora"
  vpc_id = module.vpc.vpc_id

  ingress {
    from_port       = 5432
    to_port         = 5432
    protocol        = "tcp"
    security_groups = [module.eks.cluster_primary_security_group_id]
  }

  # No egress rule: Aurora initiates no outbound connections
  # (no logical replication, no extensions that phone home).
  # Empty egress = deny-all-outbound, which is correct.
}

resource "aws_rds_cluster" "rio" {
  cluster_identifier = "${var.cluster_name}-pg"
  engine             = "aurora-postgresql"
  # 16.x: latest Aurora-supported major as of writing. Aurora lags
  # upstream PG by ~6 months. Check `aws rds describe-db-engine-
  # versions --engine aurora-postgresql` if this errors on apply.
  engine_version = "16.4"
  # "provisioned" + serverlessv2_scaling_configuration = Serverless v2.
  # engine_mode "serverless" is Serverless V1 (deprecated, don't use).
  engine_mode   = "provisioned"
  database_name = "rio"

  master_username = "rio"
  # manage_master_user_password: Aurora generates a password and
  # stores it in Secrets Manager. No sensitive values in tfstate.
  # deploy.sh fetches it via `aws secretsmanager get-secret-value`.
  manage_master_user_password = true

  db_subnet_group_name   = aws_db_subnet_group.rio.name
  vpc_security_group_ids = [aws_security_group.aurora.id]

  serverlessv2_scaling_configuration {
    # 0.5 ACU minimum = ~1 GB RAM, ~$44/mo at idle. Can't go lower
    # on Serverless v2. If this is too much, the alternative is
    # db.t3.medium provisioned (~$50/mo, no scale-down) — roughly
    # a wash for always-on dev.
    min_capacity = 0.5
    # 2 ACU max = ~4 GB RAM. Scheduler + store combined do ~10 qps
    # at peak (one INSERT per derivation per build, plus periodic
    # metric polls). 2 ACU is overkill but keeps headroom for
    # load testing.
    max_capacity = 2
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
