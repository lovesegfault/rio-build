terraform {
  backend "s3" {
    bucket       = "rio-spike-tfstate"
    key          = "spike/terraform.tfstate"
    region       = "us-east-2"
    use_lockfile = true
  }
}
