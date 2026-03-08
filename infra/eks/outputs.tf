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
