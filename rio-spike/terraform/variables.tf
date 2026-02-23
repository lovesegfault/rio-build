variable "cluster_name" {
  description = "Name of the EKS cluster"
  type        = string
  default     = "rio-spike"
}

variable "region" {
  description = "AWS region"
  type        = string
  default     = "us-east-2"
}

variable "kubernetes_version" {
  description = "EKS Kubernetes version (1.33+ required for hostUsers: false)"
  type        = string
  default     = "1.35"
}

variable "instance_type" {
  description = "EC2 instance type for spike worker nodes"
  type        = string
  default     = "c8a.xlarge"
}

variable "node_count" {
  description = "Number of tainted spike worker nodes"
  type        = number
  default     = 1
}

variable "system_node_count" {
  description = "Number of untainted nodes for system pods (coredns, etc.)"
  type        = number
  default     = 1
}
