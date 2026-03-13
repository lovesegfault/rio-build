# SSM bastion for tunneling to the internal gateway NLB.
#
# The gateway Service has `aws-load-balancer-scheme: internal` —
# the NLB gets a private DNS name resolvable only inside the VPC.
# smoke-test.sh (and humans) reach it via:
#
#   aws ssm start-session --target <instance-id> \
#     --document-name AWS-StartPortForwardingSessionToRemoteHost \
#     --parameters host=<nlb-dns>,portNumber=22,localPortNumber=2222
#
# Then `nix build --store ssh-ng://rio@localhost:2222` works.
#
# Why SSM and not an SSH bastion: no key management, no inbound SG
# rules (SSM is outbound-only — the agent connects OUT to AWS, you
# tunnel through that), session logging to CloudTrail for free.

# AL2023 AMI. The SSM agent is preinstalled and auto-starts.
data "aws_ami" "al2023" {
  most_recent = true
  owners      = ["amazon"]
  filter {
    name   = "name"
    values = ["al2023-ami-2023.*-x86_64"]
  }
}

# Instance profile wrapping the managed SSM policy. The SSM agent
# needs ssm:UpdateInstanceInformation + ssmmessages:* + ec2messages:*
# to register and accept sessions. AmazonSSMManagedInstanceCore
# is exactly that set.
resource "aws_iam_role" "bastion" {
  name = "${var.cluster_name}-bastion"
  assume_role_policy = jsonencode({
    Version = "2012-10-17"
    Statement = [{
      Effect    = "Allow"
      Principal = { Service = "ec2.amazonaws.com" }
      Action    = "sts:AssumeRole"
    }]
  })
}

resource "aws_iam_role_policy_attachment" "bastion_ssm" {
  role       = aws_iam_role.bastion.name
  policy_arn = "arn:aws:iam::aws:policy/AmazonSSMManagedInstanceCore"
}

resource "aws_iam_instance_profile" "bastion" {
  name = "${var.cluster_name}-bastion"
  role = aws_iam_role.bastion.name
}

# No inbound rules at all. SSM is outbound-only: the agent on the
# instance connects to ssm.<region>.amazonaws.com (via NAT gateway
# since this is in a private subnet), and your `aws ssm start-
# session` rides that connection. Zero attack surface.
#
# Egress: all. The bastion needs to reach the NLB (in the same VPC,
# but we don't know the NLB SG ahead of time) and SSM endpoints
# (via NAT). A tighter egress (VPC CIDR + NAT only) is possible but
# not worth the complexity for a single bastion.
resource "aws_security_group" "bastion" {
  name   = "${var.cluster_name}-bastion"
  vpc_id = module.vpc.vpc_id

  egress {
    from_port   = 0
    to_port     = 0
    protocol    = "-1"
    cidr_blocks = ["0.0.0.0/0"]
  }
}

resource "aws_instance" "bastion" {
  ami           = data.aws_ami.al2023.id
  instance_type = "t3.micro"
  # Private subnet — no public IP. SSM reaches it via the agent's
  # outbound connection through the NAT gateway.
  subnet_id                   = module.vpc.private_subnets[0]
  vpc_security_group_ids      = [aws_security_group.bastion.id]
  iam_instance_profile        = aws_iam_instance_profile.bastion.name
  associate_public_ip_address = false

  # IMDSv2 only. Same defense-in-depth as the worker nodes (though
  # the bastion runs nothing interesting — it's just a TCP forward).
  metadata_options {
    http_endpoint = "enabled"
    http_tokens   = "required"
  }

  tags = { Name = "${var.cluster_name}-bastion" }
}
