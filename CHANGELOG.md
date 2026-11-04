# Changelog

Les évolutions notables d'IdemA sont consignées ici. Le format suit [Keep a Changelog](https://keepachangelog.com/fr/1.1.0/) et les versions suivent le [versionnage sémantique](https://semver.org/lang/fr/).

## [Unreleased]

## [0.4.0] - 2026-11-04

### Modifié

- La section Bouillon de neurones est masquée par défaut. Elle s'active par une section `[ideation]` dans `config.toml` (dossier obligatoire, modèle, effort et libellé facultatifs) ; un exemple commenté figure dans la config générée. Pour retrouver le comportement d'avant : `dir = "/Users/Shared/erom-ideation"`, `model = "fable"`, `effort = "max"`, `label = "Fable"`.

## [0.3.0] - 2026-11-04

### Ajouté

- Menu Aide : « IdemA » et « IdnmA » ouvrent leur dépôt GitHub, « Signaler un problème » ouvre la création d'une issue.
- Installation en une commande : `curl -fsSL https://erom.cloud/install_idema.sh | bash` télécharge la dernière release, vérifie son empreinte, installe IdemA dans `/Applications` et propose d'installer IdnmA. Relancée, elle met IdemA à jour.

## [0.2.1] - 2026-11-04

### Modifié

- macOS 26 minimum, au lieu de macOS 14 : la version sur laquelle IdemA est construite et éprouvée.
- Le README s'ouvre sur une illustration d'IdemA.

## [0.2.0] - 2026-11-04

Première version publique.

### Ajouté

- Sessions d'agents sur un serveur tmux dédié, qui survivent à la fermeture et au plantage de l'app ; la barre latérale se reconstruit depuis tmux.
- Restauration après redémarrage du Mac : sessions, panes, dispositions et dossiers recréés, conversations Claude Code reprises.
- États des sessions (au repos, en cours, en attente, en erreur), badge du Dock et son quand un agent attend.
- Terminal `alacritty_terminal` : couleurs 24 bits, glyphes Nerd Font, neuf dispositions de 1 à 6 panes, zoom, recherche dans la sortie.
- Lanceur d'agents configurable, rangé par moteur, avec logos ; « Shell seul » intégré.
- Volet Fichiers : arbre, éditeur à onglets avec coloration, sauvegarde automatique, rechargement des fichiers réécrits par un agent, recherche et remplacement dans le projet, « Copier vers l'agent ».
- Volet Changes : diff contre la branche principale et les worktrees, indexation, commit, push, ajout au `.gitignore` ; message de commit généré par Gemini en option.
- Volet Browser : onglets WebKit, détection et pilotage des serveurs de dev, journal par serveur, ouverture des URL d'un terminal au ⌘-clic.
- Volet Projet : `AGENTS.md`, skills et serveurs MCP du projet pour Claude Code, Codex, Grok, OpenCode et Antigravity.
- Scratchpad : bloc-notes Markdown unique.
- Fenêtre Personnaliser : réglages d'IdemA, serveurs MCP, plugins, marketplaces et skills de Claude Code.
- « Ramener dans ce pane » : reprendre dans IdemA une session Claude Code lancée ailleurs.
- Commande `idema <dossier>` et action Finder « Ouvrir dans IdemA ».
- `idema --version` et ligne « IdemA <version> » du menu Aide, tirées d'une seule source, la version de `Cargo.toml` ; la commande `idema` se met à jour toute seule après une nouvelle version.
- DMG pour Apple Silicon construit par GitHub Actions à chaque tag `v*`, publié en release brouillon avec son empreinte SHA-256.
