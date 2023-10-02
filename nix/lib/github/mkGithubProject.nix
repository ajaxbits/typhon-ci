_: lib: let
  inherit
    (lib)
    mkProject
    ;
  inherit
    (lib.github)
    githubWebhook
    mkGithubJobsets
    mkGithubStatus
    ;
in {
  mkGithubProject = {
    owner,
    repo,
    secrets,
    typhon_url,
    title ? repo,
    description ? "",
    homepage ? "https://github.com/${owner}/${repo}",
    flake ? true,
  }:
    mkProject {
      meta = {inherit title description homepage;};
      actions = {
        jobsets = mkGithubJobsets {inherit owner repo flake;};
        begin = mkGithubStatus {inherit owner repo typhon_url;};
        end = mkGithubStatus {inherit owner repo typhon_url;};
        webhook = githubWebhook;
      };
      inherit secrets;
    };
}
