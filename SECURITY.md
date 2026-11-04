# Sécurité

## Signaler une vulnérabilité

Ne publiez pas une vulnérabilité dans une issue publique. Utilisez le signalement privé de GitHub : onglet **Security** du dépôt, puis **Report a vulnerability**. Décrivez la version d'IdemA (`idema --version`), la version de macOS et les étapes pour reproduire. Le signalement est traité en priorité, et vous êtes tenu au courant jusqu'au correctif.

Seule la dernière version publiée reçoit des correctifs de sécurité.

## Ce qu'IdemA fait de vos données

IdemA n'a ni télémétrie, ni statistiques d'usage, ni compte.

### Réseau

Au lancement, une seule requête part sans geste de votre part : un `git ls-remote` vers le dépôt public d'IdnmA, pour afficher sa dernière version dans le menu Aide. Elle ne transmet rien d'autre que ce que transmet un `git ls-remote`.

Tout le reste part d'un geste :

| Geste | Ce qui sort | Vers |
|---|---|---|
| Générer un message de commit (option éteinte par défaut) | le diff indexé | l'API Gemini de Google, avec votre clé |
| Installer le réseau IDNMA (menu IdemA) | téléchargement puis exécution d'un script d'installation | `erom.cloud` |
| Chercher ou ajouter une compétence (volet Projet) | la recherche, le paquet `skills` | skills.sh et le registre npm, via `bunx skills` |
| Ouvrir la galerie Artifacts | une requête avec votre session claude.ai | claude.ai |
| Naviguer dans le volet Browser | ce qu'envoie n'importe quel navigateur | les sites visités |

### Secrets

- La clé de l'API du message de commit est stockée en clair dans `~/.config/idema/config.toml`. Elle n'est jamais passée en argument d'un processus (elle transite par l'entrée standard de `curl`) et elle est masquée partout où IdemA l'affiche ou la journalise. Restreignez le fichier : `chmod 600 ~/.config/idema/config.toml`.
- Les cookies du navigateur intégré restent dans le magasin WebKit d'IdemA. IdemA n'en lit jamais la valeur.
- Le journal `~/.config/idema/logs/idema.log` ne contient ni clé, ni texte recherché, ni variables d'environnement des commandes Git.
- Les terminaux démarrent dans un environnement nettoyé : les variables de session d'un agent qui aurait lancé IdemA n'y passent pas.

### Git

IdemA ne lance que des commandes Git d'une liste blanche, et aucune commande destructive : ni `reset`, ni `checkout`, ni `clean`, ni push forcé.

### Distribution

Le DMG est construit par GitHub Actions à partir du tag de la release, signé ad hoc et non notarisé par Apple. Chaque release publie l'empreinte SHA-256 du DMG.
