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
  value       = module.rio_store_irsa.iam_role_arn
}

output "oidc_provider_arn" {
  description = "OIDC provider ARN (for additional IRSA roles if needed)"
  value       = module.eks.oidc_provider_arn
}

output "ecr_registry" {
  description = "ECR registry hostname (<account>.dkr.ecr.<region>.amazonaws.com). push-images.sh and deploy.sh both read this."
  value       = local.ecr_registry
}

output "chunk_bucket_name" {
  description = "S3 bucket for NAR chunks (deploy.sh sets RIO_CHUNK_BACKEND__BUCKET to this)"
  value       = aws_s3_bucket.chunks.bucket
}

output "region" {
  description = "AWS region (scripts read this so they don't have to hardcode it)"
  value       = var.region
}
