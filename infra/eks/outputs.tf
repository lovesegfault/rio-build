output "cluster_name" {
  description = "EKS cluster name"
  value       = module.eks.cluster_name
}

output "cluster_endpoint" {
  description = "EKS API server endpoint"
  value       = module.eks.cluster_endpoint
}

output "kubeconfig_command" {
  description = "Run this to configure kubectl for the cluster"
  value       = "aws eks update-kubeconfig --region ${var.region} --name ${module.eks.cluster_name}"
}

output "store_iam_role_arn" {
  description = "IAM role ARN for rio-store IRSA (set STORE_IAM_ROLE_ARN to this before kustomize apply)"
  value       = module.rio_store_irsa.arn
}

output "bootstrap_iam_role_arn" {
  description = "IAM role ARN for the rio-bootstrap Job IRSA (helm pre-install hook that seeds rio/* secrets in Secrets Manager)"
  value       = module.rio_bootstrap_irsa.arn
}

output "oidc_provider_arn" {
  description = "OIDC provider ARN (for additional IRSA roles if needed)"
  value       = module.eks.oidc_provider_arn
}

output "ecr_registry" {
  description = "ECR registry hostname (<account>.dkr.ecr.<region>.amazonaws.com). push-images.sh and `just eks deploy` both read this."
  value       = local.ecr_registry
}

output "chunk_bucket_name" {
  description = "S3 bucket for NAR chunks (`just eks deploy` passes as --set store.chunkBackend.bucket)"
  value       = aws_s3_bucket.chunks.bucket
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
  description = "Secrets Manager ARN for the Aurora master password (`just eks deploy` passes to the chart; ESO builds the connection string)"
  value       = aws_rds_cluster.rio.master_user_secret[0].secret_arn
}

output "bastion_instance_id" {
  description = "SSM bastion instance ID (smoke-test.sh uses this for the port-forward session)"
  value       = aws_instance.bastion.id
}
