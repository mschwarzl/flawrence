---
name: cloud-iac
description: "Cloud and infrastructure-as-code security: Terraform/CloudFormation, Kubernetes manifests, IAM policies, container security. Load when the target contains IaC, K8s YAML, IAM policies or Dockerfiles."
---

# Cloud and IaC security

When the target contains Terraform/CloudFormation, Kubernetes manifests, IAM policies or Dockerfiles:

## IAM and policies
- Wildcard principals or actions (`"*"`, `*:*`) in roles/trust policies
- Trust policies allowing any account (`Principal: "*"`), or `sts:ExternalId` missing on third-party role assumption
- Overly broad condition keys; `aws:SourceArn` missing on resource-based policies
- Secrets in plaintext variables, outputs, or state files

## Kubernetes
- Pods with `privileged: true`, `hostNetwork`/`hostPID`, or `runAsUser: 0`
- Missing `readOnlyRootFilesystem`, `allowPrivilegeEscalation: false`, dropped capabilities
- `ClusterRoleBinding` to `cluster-admin` for workloads that don't need it
- Secrets as env vars instead of mounted files; `default` service account with automount
- Ingress without TLS; network policies absent between namespaces

## Containers and IaC
- `latest` tags, root user in Dockerfile, `ADD` of remote URLs, secrets in build args
- Public S3 buckets / storage blobs, open security groups (0.0.0.0/0 on 22/3389/RDP)
- Unencrypted data stores; public RDS/DB snapshots
- Terraform state over unencrypted remote backend

## When reviewing
- The exploit primitive is usually "this workload can reach/act on X" — state the reachable resource and the action allowed.
