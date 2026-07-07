# Security Policy

## Supported Versions

We provide security updates for the following versions of workspace-qdrant-mcp:

| Version | Supported          | End of Life    |
| ------- | ------------------ | -------------- |
| 0.1.x   | ✅ Yes             | TBA            |
| < 0.1   | ❌ No              | Ended          |

**Node.js Compatibility:** Node.js 18+ is required for the TypeScript MCP server.

**Rust Compatibility:** Rust 1.75+ is required for the daemon and CLI.

**Qdrant Compatibility:** Qdrant 1.7+ is required for full security feature support.

## Reporting Security Vulnerabilities

We take security vulnerabilities seriously. Please follow our responsible disclosure process:

### 🚨 **For Sensitive/Critical Issues**

**Email:** [alberto.souza@althoztecnologia.com.br](mailto:alberto.souza@althoztecnologia.com.br)

- Use this for vulnerabilities that could be exploited maliciously
- Include "SECURITY" in the subject line
- We'll acknowledge receipt within 48 hours
- Initial assessment provided within 1 week

### 📋 **For General Security Issues**

Use GitHub's Security Advisory system for coordinated disclosure:

1. Go to the **Security** tab in this repository
2. Click **Report a vulnerability**
3. Fill out the private security advisory form

This allows for private collaboration with our team before public disclosure.

### 🔍 **For Non-Sensitive Security Improvements**

Use our [Security Issue Template](https://github.com/alkmimm/workspace-qdrant-mcp/issues/new?template=security.yml) for:
- Security configuration improvements
- Documentation security updates
- General security hardening suggestions

## Response Timeline

| Severity | Acknowledgment | Initial Assessment | Resolution Target |
|----------|---------------|-------------------|------------------|
| Critical | 24 hours      | 48 hours         | 30 days          |
| High     | 48 hours      | 1 week           | 60 days          |
| Medium   | 72 hours      | 2 weeks          | 90 days          |
| Low      | 1 week        | 4 weeks          | Next release     |

## Security Guidelines for Users

### 🔐 **API Key Management**

**Qdrant Cloud Security:**
```bash
# Use environment variables, never hardcode keys
export QDRANT_API_KEY="your-secure-key"

# For production, use secrets management
kubectl create secret generic qdrant-secret --from-literal=api-key=your-key
```

**Local Development:**
```bash
# Secure your local Qdrant instance
docker run -p 6333:6333 \
  -e QDRANT__SERVICE__HTTP__ENABLE_CORS=false \
  qdrant/qdrant
```

### 🛡️ **Network Security**

**Production Deployment:**
- Always use HTTPS/TLS for Qdrant connections
- Implement proper firewall rules
- Use VPN or private networks for cloud deployments

**Configuration:**
```json
{
  "mcpServers": {
    "workspace-qdrant-mcp": {
      "env": {
        "QDRANT_URL": "https://your-secure-qdrant.com:6334",
        "QDRANT_API_KEY": "${QDRANT_API_KEY}"
      }
    }
  }
}
```

### 📊 **Data Privacy**

**Sensitive Information:**
- Never store personally identifiable information (PII) in embeddings
- Review documents before ingestion for sensitive data
- Use collection-level access controls when available

**Embedding Model Considerations:**
- Local models (recommended): Data stays on your system
- Cloud models: Review privacy policies for your use case

### 🏗️ **Secure Development**

**For Contributors:**
- Run security tests: `workspace-qdrant-test --security`
- Check dependencies: `pip-audit`
- Validate configurations: `workspace-qdrant-validate`

## Security Features

### 🔒 **Built-in Security**

- **Input Validation:** All user inputs are sanitized and validated
- **Dependency Scanning:** Automated vulnerability scanning in CI/CD
- **Security Testing:** Comprehensive security test suite
- **Secure Defaults:** Conservative configuration defaults

### 📝 **Security Monitoring**

Our continuous security monitoring includes:

- **Automated Dependency Updates:** Via Dependabot
- **Vulnerability Scanning:** GitHub Security Advisories
- **Code Security Analysis:** CodeQL and security linting
- **Third-party Security Assessment:** [MseeP.ai verified](https://mseep.ai/app/chrisgve-workspace-qdrant-mcp)

### 🛠️ **Diagnostic Tools**

```bash
# Security-focused diagnostics
workspace-qdrant-test --component security
workspace-qdrant-health --security-check
workspace-qdrant-validate --security
```

## Security Updates

### 📢 **Stay Informed**

- **GitHub Releases:** Subscribe to release notifications
- **Security Advisories:** Watch this repository for security updates
- **Changelog:** Review CHANGELOG.md for security-related changes

### 🔄 **Update Process**

```bash
# Check current version
workspace-qdrant-mcp --version

# Update to latest secure version
pip install --upgrade workspace-qdrant-mcp

# Verify installation
workspace-qdrant-test --quick
```

## Scope

This security policy covers:

- **workspace-qdrant-mcp** package and all included tools
- **Configuration templates** and examples
- **Documentation** and setup guides
- **CI/CD workflows** and automation

## Recognition

We appreciate security researchers and contributors who help improve our security posture. Contributors who report valid security issues will be:

- Acknowledged in our security changelog (with permission)
- Credited in the GitHub Security Advisory
- Listed in our CONTRIBUTORS.md file

## Legal

This security policy complements but does not replace our [License](LICENSE). For questions about responsible disclosure or this policy, contact [christian.berclaz@mac.com](mailto:christian.berclaz@mac.com).

---

**Last Updated:** September 1, 2025
**Next Review:** December 1, 2025

For questions about this security policy, please email [alberto.souza@althoztecnologia.com.br](mailto:alberto.souza@althoztecnologia.com.br) or open a [general discussion](https://github.com/alkmimm/workspace-qdrant-mcp/discussions).