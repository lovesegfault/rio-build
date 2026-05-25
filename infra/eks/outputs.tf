output "cluster_name" {
  description = "EKS cluster name"
  value       = module.eks.cluster_name
}

output "kubeconfig_command" {
  description = "Run this to configure kubectl for the cluster"
  value       = "aws eks update-kubeconfig --region ${var.region} --name ${module.eks.cluster_name}"
}

output "store_iam_role_arn" {
  description = "IAM role ARN for rio-store IRSA (xtask passes as helm --set store.serviceAccount.annotations)"
  value       = module.rio_store_irsa.arn
}

output "scheduler_iam_role_arn" {
  description = "IAM role ARN for rio-scheduler IRSA (xtask passes as --set scheduler.serviceAccount.annotations)"
  value       = module.rio_scheduler_irsa.arn
}

output "bootstrap_iam_role_arn" {
  description = "IAM role ARN for the rio-bootstrap Job IRSA (helm pre-install hook that seeds rio/* secrets in Secrets Manager)"
  value       = module.rio_bootstrap_irsa.arn
}

output "ecr_registry" {
  description = "ECR registry hostname (<account>.dkr.ecr.<region>.amazonaws.com). xtask push and deploy both read this."
  value       = local.ecr_registry
}

output "chunk_bucket_name" {
  description = "S3 bucket for NAR chunks (xtask passes as --set store.chunkBackend.bucket)"
  value       = aws_s3_bucket.chunks.bucket
}

output "express_bucket_by_az_id" {
  description = "Per-AZ S3 Express directory bucket names keyed by physical AZ-ID (use2-az1 → rio-build-chunk-cache--use2-az1--x-s3). Empty map = cache tier disabled. xtask passes as --set-json store.chunkBackend.expressBucketByAzId; the store-side per-pod selection resolves its own AZ-ID against this map (P0554)."
  value       = { for az_id, b in aws_s3_directory_bucket.cache : az_id => b.bucket }
}

output "express_bucket_by_zone" {
  description = "Same buckets keyed by AZ NAME (us-east-2a → ...--x-s3). The name→ID mapping is account-specific and terraform is the only layer that knows it; this is the map to use if the per-pod selection keys off the node's topology.kubernetes.io/zone label instead of IMDS placement/availability-zone-id."
  value       = { for az_id, b in aws_s3_directory_bucket.cache : local.zone_name_by_az_id[az_id] => b.bucket }
}

output "region" {
  description = "AWS region (scripts read this so they don't have to hardcode it)"
  value       = var.region
}

output "db_endpoint" {
  description = "Aurora cluster writer endpoint (hostname only, no port)"
  value       = aws_rds_cluster.rio.endpoint
}

output "db_secret_arn" {
  description = "Secrets Manager ARN for the Aurora master password (xtask passes to the chart; ESO builds the connection string)"
  value       = aws_rds_cluster.rio.master_user_secret[0].secret_arn
}

output "vpc_id" {
  description = "VPC ID — destroy.rs ENI/SG sweep filters on this (was missing → sweep silently no-op'd)"
  value       = module.vpc.vpc_id
}

output "vpc_ipv6_cidr_block" {
  description = "VPC IPv6 /56 (xtask passes as --set global.postgresCidr so the store-egress CiliumNetworkPolicy admits the Aurora AAAA endpoint)"
  value       = module.vpc.vpc_ipv6_cidr_block
}

output "gateway_dns_fqdn" {
  description = "Stable FQDN for rio-gateway (xtask annotates the Service with this for external-dns). Empty when gateway_dns is disabled."
  value       = local.dns_enabled ? local.gateway_dns_fqdn : ""
}

output "karpenter_node_role_name" {
  description = "Node IAM role name for Karpenter-provisioned instances (goes into EC2NodeClass.spec.role — xtask passes as --set karpenter.nodeRoleName)"
  value       = module.karpenter.node_iam_role_name
}
