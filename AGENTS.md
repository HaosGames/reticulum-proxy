# AGENTS.md - Für KI-Assistenten (ArgoCD/Talos)

## Repository-Kontext

Dies ist ein **GitOps-Repository** für einen bare-metal Kubernetes Cluster basierend auf Talos Linux.

### Architektur
- **Orchestrierung**: ArgoCD (GitOps-Prinzip)
- **OS**: Talos Linux mit talhelper
- **Storage**: Longhorn (Cluster), JuiceFS (Network), Garage (S3)
- **Networking**: MetalLB, Tailscale (Ingress)
- **Secrets**: Bitnami SealedSecrets
- **Monitoring**: VictoriaMetrics
- **Backups**: VolSync, talos-backup, restic

### Verzeichnisstruktur

```
clusters/
  CLUSTER_NAME/
    kustomization.yaml          # Root Application -> enthaelt ApplicationSets

helm/                           # Helm-basierte Applikationen
  CHART_NAME/
    values.yaml                 # Default values
    CLUSTER_NAME-values.yaml    # Cluster-spezifische Werte (generiert App)

helm-no-namespace/              # Helm Charts ohne Namespace (Cluster-Resources)
  CHART_NAME/
    values.yaml
    CLUSTER_NAME-values.yaml

kustomize/                      # Kustomize-basierte Applikationen
  PROJECT/
    CLUSTER_NAME/               # Existiert -> generiert App fuer Cluster
      kustomization.yaml

kustomize-server-side/          # Server-side apply (fuer CRDs etc.)
  PROJECT/
    CLUSTER_NAME/
      kustomization.yaml

talos/
  CLUSTER_NAME/                 # Talos Linux Konfiguration
    talconfig.yaml
    talsecret.yaml              # NICHT im Git (in .gitignore)
```

### ApplicationSets Logik

ArgoCD ApplicationSets durchsuchen die Verzeichnisse und generieren automatisch Applications:

1. **helm/**: Fuer jede CLUSTER-values.yaml wird eine Helm-Application erstellt
2. **helm-no-namespace/**: Gleich, aber ohne namespace-Angabe
3. **kustomize/**: Fuer jeden Cluster-Subfolder wird eine Kustomize-Application erstellt
4. **kustomize-server-side/**: Gleich, aber mit server-side: true

### Wichtige Befehle

```bash
# Talos Config generieren
cd talos/CLUSTER
talhelper genconfig

# Talos Node upgrade (immer --preserve!)
talosctl upgrade --preserve -n NODE_IP --image INSTALLER_URL

# Sealed Secret erstellen
kubeseal -f secret.yaml -w sealed-secret.yaml
```

### Hosted Applications

- **Immich** – Foto/Video Management
- **PaperlessNgx** – Dokumentenmanagement
- **Actual Budget** – Finanzverwaltung

### Sicherheitshinweise

- talsecret.yaml niemals committen (in .gitignore)
- SealedSecrets-Keys sicher backupen (main.key)
- Backup-Keys regelmaessig nach Renewal aktualisieren

### Gemeinsame Aufgaben

- Neue Apps via Helm/Kustomize hinzufuegen
- Cluster-spezifische Werte in *-values.yaml anpassen
- Talos-Konfigurationen aktualisieren
- SealedSecrets erstellen/rotieren

---

*Diese Datei hilft KI-Assistenten, sich im Repo zurechtzufinden.*
