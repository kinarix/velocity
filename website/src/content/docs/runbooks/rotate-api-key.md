---
title: Rotate API Key
description: Replace expired or compromised API keys
---

Rotate API keys every 90 days or immediately if compromised.

## Why Rotate

- **Expiration:** TTL reached (90 days default)
- **Compromise:** Key leaked in logs, GitHub, etc.
- **Revocation:** Service no longer needs access
- **Compliance:** Security policy requires rotation

## Process

### 1. Create New Key

```bash
velocity api-key create \
  --name ci-deploy-2026-06 \
  --ttl 90d \
  --scope region=west,store_ids=10:20:30
```

Output:

```
vel_ci-deploy-2026-06_abc123def456xyz...
SAVE THIS NOW — you will not see it again.
```

**Critical:** Copy the plaintext immediately. The CLI never stores or retrieves it.

### 2. Store in Secret Manager

Add the new key to your secret manager (not Git):

```bash
# GitHub Secrets
gh secret set VELOCITY_API_KEY -b "vel_ci-deploy-2026-06_abc123def456xyz..."

# AWS Secrets Manager
aws secretsmanager put-secret-value \
  --secret-id velocity-api-key \
  --secret-string '{"key":"vel_ci-deploy-2026-06_abc123def456xyz..."}'

# HashiCorp Vault
vault kv put secret/velocity/api-key key="vel_ci-deploy-2026-06_abc123def456xyz..."
```

### 3. Update Deployments

Update all places using the old key:

**GitHub Actions workflow:**

```yaml
jobs:
  deploy:
    runs-on: ubuntu-latest
    steps:
      - uses: actions/checkout@v4
      - name: Deploy
        env:
          VELOCITY_API_KEY: ${{ secrets.VELOCITY_API_KEY }}
        run: |
          curl -H "X-API-Key: $VELOCITY_API_KEY" \
            https://api.velocity.acme.com/api/acme/supply-chain/...
```

**Kubernetes CronJob:**

```yaml
apiVersion: batch/v1
kind: CronJob
metadata:
  name: velocity-sync
spec:
  schedule: "0 2 * * *"
  jobTemplate:
    spec:
      template:
        spec:
          serviceAccountName: velocity-sync
          containers:
          - name: sync
            image: curlimages/curl:latest
            env:
            - name: VELOCITY_API_KEY
              valueFrom:
                secretKeyRef:
                  name: velocity-api-key
                  key: key
            command: ["/bin/sh"]
            args:
            - -c
            - |
              curl -H "X-Api-Key: $VELOCITY_API_KEY" \
                https://api.velocity.acme.com/api/acme/...
```

**Docker environment variable:**

```bash
docker run \
  -e VELOCITY_API_KEY="vel_ci-deploy-2026-06_abc123def456xyz..." \
  my-service:latest
```

### 4. Verify New Key Works

Test the new key before revoking the old one:

```bash
# Test new key
curl -H "X-API-Key: vel_ci-deploy-2026-06_abc123def456xyz..." \
  https://api.velocity.acme.com/api/acme/supply-chain/procurement/purchase-order/v1

# Expected: 200 OK with data
```

### 5. Revoke Old Key

Once verified, revoke the old key:

```bash
velocity api-key revoke --name ci-deploy-2025-12

# Verify revocation
curl -H "X-API-Key: vel_ci-deploy-2025-12_oldkey..." \
  https://api.velocity.acme.com/api/acme/supply-chain/procurement/purchase-order/v1

# Expected: 401 Unauthorized (key revoked)
```

### 6. Update Documentation

Update team wiki with new key expiration date:

```markdown
## API Key Rotation Schedule

| Name               | Expires      | Last Rotated |
|--------------------|--------------|--------------|
| ci-deploy-2026-06  | 2026-09-18   | 2026-06-19   |
| data-sync-2026-06  | 2026-09-15   | 2026-06-15   |
```

## Automation (Optional)

### GitHub Actions: Auto-Rotate on Schedule

```yaml
name: Rotate API Key
on:
  schedule:
    - cron: '0 0 1 * *'  # First day of month

jobs:
  rotate:
    runs-on: ubuntu-latest
    steps:
      - uses: actions/checkout@v4
      
      - name: Create new key
        id: new_key
        run: |
          KEY=$(velocity api-key create --name ci-deploy-$(date +%Y-%m) --ttl 90d)
          echo "key=$KEY" >> $GITHUB_OUTPUT
      
      - name: Update secret
        env:
          GH_TOKEN: ${{ secrets.GH_TOKEN }}
        run: |
          gh secret set VELOCITY_API_KEY -b "${{ steps.new_key.outputs.key }}"
      
      - name: List old keys
        run: |
          velocity api-key list | tail -5
      
      - name: Notify team
        run: |
          curl -X POST ${{ secrets.SLACK_WEBHOOK }} \
            -d '{"text":"API key rotated. Old key expires in 90 days."}'
```

### Kubernetes CronJob: Auto-Rotate

```yaml
apiVersion: batch/v1
kind: CronJob
metadata:
  name: velocity-api-key-rotate
  namespace: velocity-system
spec:
  schedule: "0 0 1 * *"  # First day of month
  jobTemplate:
    spec:
      template:
        spec:
          serviceAccountName: velocity-admin
          containers:
          - name: rotate
            image: curlimages/curl:latest
            command:
            - /bin/sh
            - -c
            - |
              # Assume velocity CLI is installed
              NEW_KEY=$(velocity api-key create --name ci-deploy-$(date +%Y-%m) --ttl 90d)
              kubectl patch secret velocity-api-key -p "{\"data\":{\"key\":\"$(echo -n $NEW_KEY | base64)\"}}"
          restartPolicy: OnFailure
```

## Emergency Revocation

If key is compromised immediately:

```bash
velocity api-key revoke --name ci-deploy-2026-06

# Verify revocation (within seconds)
curl -H "X-API-Key: vel_ci-deploy-2026-06_abc123def456xyz..." \
  https://api.velocity.acme.com/api/acme/supply-chain/procurement/purchase-order/v1

# Expected: 401 Unauthorized
```

No key regeneration needed; just create a new one.

## Monitoring

Alert on key usage anomalies:

```bash
# Check last-used timestamp
velocity api-key list | grep ci-deploy-2026-06

# Alert if key hasn't been used in 30 days (may be forgotten)
# Alert if key is used from unexpected IP (possible theft)
```

## Checklist

- [ ] Created new API key
- [ ] Stored in secret manager (not plaintext in Git)
- [ ] Updated all deployments/scripts
- [ ] Tested new key works (200 OK response)
- [ ] Revoked old key
- [ ] Updated team documentation
- [ ] Verified revocation (401 on old key)
- [ ] Notified team of rotation date
- [ ] Scheduled next rotation (90 days ahead)

