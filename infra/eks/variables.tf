variable "region" {
  description = "AWS region"
  type        = string
  default     = "us-west-2"
}

variable "cluster_name" {
  description = "EKS cluster name (also used as prefix for IAM roles etc)"
  type        = string
  default     = "rio-build"
}

variable "kubernetes_version" {
  description = "K8s version for the EKS control plane"
  type        = string
  default     = "1.31"
}

variable "system_instance_type" {
  description = "Instance type for system nodegroup (scheduler/store/gateway/controller)"
  type        = string
  default     = "m5.large"
}

variable "worker_instance_type" {
  description = "Instance type for worker nodegroup (rio-worker pods, compute-heavy)"
  type        = string
  # c6a = AMD EPYC, compute-optimized. Good price/perf for nix builds.
  default = "c6a.xlarge"
}

variable "worker_min_size" {
  description = "Minimum worker nodes"
  type        = number
  default     = 2
}

variable "worker_max_size" {
  description = "Maximum worker nodes (autoscaler ceiling)"
  type        = number
  default     = 10
}

variable "chunk_bucket" {
  description = "S3 bucket name for NAR chunk storage (must pre-exist)"
  type        = string
}
