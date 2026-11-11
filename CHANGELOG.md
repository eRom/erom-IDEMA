# Changelog

Les évolutions notables d'IdemA sont consignées ici. Le format suit [Keep a Changelog](https://keepachangelog.com/fr/1.1.0/) et les versions suivent le [versionnage sémantique](https://semver.org/lang/fr/).

## [Unreleased]

## [0.6.3] - 2026-11-11

### Modifié

- Barre latérale : l'état de connexion de Localhost et de chaque machine distante s'affiche par une icône d'écran, verte quand la machine est connectée, grisée sinon, au lieu d'une pastille qui se confondait avec celles des sessions.

## [0.6.2] - 2026-11-11

### Corrigé

- Compagnon iPhone : les bordures du champ de saisie de Claude Code tiennent sur une seule ligne, au lieu de s'étaler sur plusieurs lignes quand le terminal du Mac est large. Le nom de session reste à droite comme sur le Mac. Les tableaux, la prose et le mode sans retour à la ligne s'affichent comme avant.

## [0.6.1] - 2026-11-09

### Modifié

- Compagnon iPhone : l'onglet Notes devient un vrai éditeur du Scratchpad. Toute la note se modifie en texte brut (supprimer, insérer au milieu, coller), avec un enregistrement automatique après une courte pause. Une barre collée au clavier ajoute `#`, `-`, retrait, indentation et un bloc de code. Sans le Mac, la frappe reste sur le téléphone et part seule à son retour, même si l'onglet Notes est fermé. Si la note a changé sur le Mac entre-temps, rien n'est écrasé : un bandeau propose de reprendre la note du Mac ou d'écraser avec le brouillon.

## [0.6.0] - 2026-11-08

### Ajouté

- Machines distantes (Personnaliser › Machines) : IdemA pilote par ssh le tmux d'un serveur ou d'un autre ordinateur. Chaque machine a sa section repliable dans la barre latérale, avec toutes ses sessions tmux (celles ouvertes à la main comprises), leur état, la sonnerie et le lanceur d'agents lu dans le `~/.idnma.yml` de la machine. IdemA fixe la taille des fenêtres qu'il affiche. Une coupure ne recrée rien : la saisie est bloquée et signalée pendant la perte, les vues se rattachent seules avec leur historique, tout de suite au retour du réseau. Le volet Terminal ouvre un shell sur la machine ; le Scratchpad est partagé ; les autres volets ne sont pas encore disponibles à distance. Le compagnon iPhone voit et notifie aussi les sessions distantes. Prérequis : tmux 3.8 sur la machine, une connexion ssh sans mot de passe et une clé d'hôte déjà acceptée.

## [0.5.0] - 2026-11-06

### Ajouté

- Compagnon iPhone (éteint par défaut, section `[companion]` ou Personnaliser › Compagnon) : une page web installable sur l'écran d'accueil montre vos sessions, leur écran et leurs notes, accepte votre saisie, et envoie une notification quand un agent attend. Appairage par un code à 6 chiffres, exposition sur votre tailnet par `tailscale serve`, notifications chiffrées de bout en bout vers l'appareil. Rien ne part si le Mac est en veille ou IdemA fermé. Après une mise à jour qui change les balises de la page, supprimez l'icône de l'écran d'accueil, videz les données du site dans Safari, puis ajoutez-la de nouveau : iOS lit ces réglages à l'installation.

### Corrigé

- Les éditeurs (volet Fichiers, Scratchpad) ne gèlent plus sur les gros fichiers : un fichier de 1 Mo s'ouvre en un dixième de seconde au lieu de 3, et changer la largeur du volet ne fige plus l'interface près de 2 s. Sur le texte et le code courants, les lignes se coupent aux mêmes endroits qu'avant.

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
