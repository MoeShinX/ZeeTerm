use super::*;

/// 这些都必须被认出来。
const SECRET: &[&str] = &[
    // -p 当密码的命令
    "mysql -u root -p",
    "mysql -u root -p hunter2",
    "mysql -u root -phunter2",
    "mysql -u root -p'hunter2'",
    "mysql --password=hunter2 -u root",
    "sudo mysql -uroot -pSecret",
    "mysqldump -h db -u root -pSecret app > dump.sql",
    "mariadb -p",
    "sshpass -p hunter2 ssh deploy@build-box",
    "7z a -pSecret out.7z data",
    "mongosh -u admin -p hunter2",
    "useradd -p '$6$salt$hash' bob",
    "unzip -P hunter2 archive.zip",
    "sqlcmd -S db -U sa -P hunter2",
    "redis-cli -h cache -a hunter2",
    "ldapsearch -x -D cn=admin -w hunter2",
    // 长选项
    "app --password hunter2",
    "app --password=hunter2",
    "app --Password=hunter2",
    "app --passwd=x",
    "app --passphrase hunter2",
    "app --token abc123",
    "app --token=abc123",
    "app --access-token=abc123",
    "app --api-key abc",
    "app --apikey=abc",
    "app --secret=abc",
    "app --client-secret abc",
    "app --auth-token abc",
    "app --auth abc",
    "app --authorization abc",
    "app --credentials abc",
    "wget --http-password=abc http://h/f",
    "docker login -u me --password-stdin",
    // curl 的 -u / --user 带密码
    "curl -u admin:hunter2 https://api.example.com",
    "curl -uadmin:hunter2 https://api.example.com",
    "curl --user admin:hunter2 https://api.example.com",
    "curl --user=admin:hunter2 https://api.example.com",
    // openssl / rdp
    "openssl enc -aes-256-cbc -pass pass:hunter2 -in a -out b",
    "openssl rsa -passin pass:hunter2 -in k.pem",
    "xfreerdp /v:host /u:me /p:hunter2",
    // 认证头
    "curl -H 'Authorization: Bearer abc' https://api.example.com",
    "curl -H \"authorization : abc\" https://api.example.com",
    "curl -H 'Proxy-Authorization: Basic abc' https://api.example.com",
    "curl -H 'X-API-Key: abc' https://api.example.com",
    "curl -H 'X-Auth-Token: abc' https://api.example.com",
    "curl -H 'Cookie: sid=abc' https://api.example.com",
    "curl --header Bearer abc https://api.example.com",
    // 秘密环境赋值
    "AWS_SECRET_ACCESS_KEY=abc aws s3 ls",
    "export AWS_SECRET_ACCESS_KEY=abc",
    "export AWS_SECRET_ACCESS_KEY=",
    "GITHUB_TOKEN=abc git push",
    "export DB_PASSWORD=abc",
    "export MY_PASSWD=abc",
    "PGPASSWORD=abc psql -h db",
    "MYSQL_PWD=abc mysql -h db",
    "export api_key=abc",
    "export STRIPE_API_KEY=abc",
    "export PRIVATE_KEY=abc",
    "export SOME_CREDENTIALS=abc",
    "docker run -e DB_PASSWORD=abc img",
    "docker run --env=API_TOKEN=abc img",
    "kubectl create secret generic x --from-literal=password=abc",
    "env FOO=1 BAR_SECRET=abc ./run.sh",
    // 带凭据的 URL
    "git clone https://user:hunter2@github.com/org/repo.git",
    "curl https://user:hunter2@example.com/path",
    "psql postgres://app:hunter2@db:5432/app",
    "redis-cli -u redis://:hunter2@cache:6379",
    "wget ftp://anon:pw@host/file",
    // 查询串里的令牌
    "curl 'https://api.example.com/v1?token=abc'",
    "curl \"https://api.example.com/v1?a=1&api_key=abc\"",
    "curl 'https://x.example.com/?password=abc'",
    // 令牌形态
    "echo ghp_abcdefghijklmnopqrstuvwxyz0123456789",
    "echo github_pat_11ABCDEFG0123456789_abcdefghijklmnop",
    "export X=sk-abcdefghijklmnopqrstuvwxyz",
    "aws configure set aws_access_key_id AKIAIOSFODNN7EXAMPLE",
    "echo xoxb-1234567890-abcdefghij",
    "git remote add o https://oauth2:glpat-abcdefghijklmnopqr@gitlab.example.com/x.git",
    "curl -H x:eyJhbGciOiJIUzI1NiJ9.eyJzdWIiOiIxMjM0NTY3ODkwIn0.abcdefghijklmnop",
    // 控制字符 / 换行
    "echo a\necho b",
    "echo \u{1b}[31mred",
    "echo a\tb",
    "echo a\r",
    // 后面的命令段里
    "cd /srv && mysql -u root -pSecret",
    "echo hi | mysql -pSecret",
    "ls; export TOKEN=abc",
];

/// 这些都不能被误伤：常见的、不含秘密的命令。
const PLAIN: &[&str] = &[
    "ls -la",
    "ls -p",
    "cd /srv/www",
    "ssh -p 2222 deploy@build-box",
    "ssh deploy@build-box",
    "scp -P 2222 file.txt build-box:/tmp",
    "scp -p file.txt build-box:/tmp",
    "docker run -p 80:80 nginx",
    "docker run -d -p 8080:80 --name web nginx",
    "docker run -u 1000:1000 img",
    "docker run --user 1000:1000 img",
    "docker run --user www-data:www-data img",
    "docker ps -a",
    "mkdir -p /var/www/app",
    "cp -p a b",
    "git add -p",
    "git commit --author=\"Ann <ann@example.com>\" -m wip",
    "git log --author=ann",
    "git clone git@github.com:org/repo.git",
    "git clone ssh://git@github.com/org/repo.git",
    "git clone https://github.com/org/repo.git",
    "ps -p 1234",
    "kill -9 1234",
    "nmap -p 80,443 host",
    "psql -p 5433 -U me db",
    "psql -h db -U me -d app",
    "mysql -u root",
    "mysql -u root -h db --port 3306",
    "mysqldump --single-transaction app",
    "sudo -u postgres psql",
    "sudo systemctl restart nginx",
    "curl -u admin https://api.example.com",
    "curl -s https://example.com/index.html",
    "curl -X POST -H 'Content-Type: application/json' -d '{}' https://example.com",
    "curl 'https://example.com/search?q=rust&page=2'",
    "export PATH=$PATH:/usr/local/bin",
    "export EDITOR=vim",
    "LANG=C ls",
    "tail -n 50 /var/log/auth.log",
    "cat /etc/passwd",
    "passwd bob",
    "grep -i password config.yml",
    "kubectl get secrets",
    "kubectl get pods -n kube-system",
    "find . -name '*.log' -print",
    "find . -perm 644 -path ./x -prune",
    "tar -xzf backup.tar.gz",
    "unzip archive.zip",
    "ls --color=auto",
    "echo token",
    "echo bearer",
    "redis-cli -h cache ping",
    "systemctl status ssh",
    "journalctl -u nginx -n 100",
    "top -p 1234",
    "rsync -avz -e 'ssh -p 2222' ./dist build-box:/srv",
];

#[test]
fn every_known_secret_form_is_recognised() {
    let missed: Vec<&&str> = SECRET.iter().filter(|cmd| !is_sensitive(cmd)).collect();
    assert!(missed.is_empty(), "漏掉了: {missed:#?}");
}

#[test]
fn ordinary_commands_are_not_flagged() {
    let flagged: Vec<(&&str, Option<SensitiveReason>)> = PLAIN
        .iter()
        .map(|cmd| (cmd, sensitivity(cmd)))
        .filter(|(_, reason)| reason.is_some())
        .collect();
    assert!(flagged.is_empty(), "误伤了: {flagged:#?}");
}

#[test]
fn reasons_name_the_rule_that_fired() {
    assert_eq!(
        sensitivity("echo a\nb"),
        Some(SensitiveReason::Control),
        "换行"
    );
    assert_eq!(
        sensitivity("app --password=x"),
        Some(SensitiveReason::SecretFlag)
    );
    assert_eq!(
        sensitivity("mysql -pSecret"),
        Some(SensitiveReason::SecretFlag)
    );
    assert_eq!(
        sensitivity("curl -H 'Authorization: x' u"),
        Some(SensitiveReason::AuthHeader)
    );
    assert_eq!(
        sensitivity("export AWS_SECRET_ACCESS_KEY=1"),
        Some(SensitiveReason::SecretEnv)
    );
    assert_eq!(
        sensitivity("git clone https://u:p@h/r"),
        Some(SensitiveReason::CredentialUrl)
    );
    assert_eq!(
        sensitivity("echo ghp_abcdefghijklmnopqrstuvwxyz0123456789"),
        Some(SensitiveReason::TokenShape)
    );
}

#[test]
fn the_rules_only_look_at_the_text_and_are_case_insensitive_where_they_should_be() {
    assert!(is_sensitive("APP --PASSWORD=x"));
    assert!(is_sensitive("curl -H 'AUTHORIZATION: x' u"));
    assert!(is_sensitive("export github_token=x"));
    // 大小写敏感的是令牌形态的前缀和 -p / -P 的区别。
    assert!(
        !is_sensitive("unzip -p archive.zip file"),
        "unzip -p 是输出到屏幕"
    );
    assert!(is_sensitive("unzip -P secret archive.zip"));
}

#[test]
fn a_secret_hidden_in_a_quoted_or_escaped_word_is_still_found() {
    assert!(is_sensitive("mysql -u root \"-pSecret\""));
    assert!(is_sensitive("app '--password=hunter2'"));
    assert!(is_sensitive("app --pass\\word=x"));
    assert!(is_sensitive("echo 'a'; export TOKEN='x y'"));
}

#[test]
fn the_reason_text_never_contains_the_command() {
    let reason = sensitivity("mysql -pSuperSecret99").expect("敏感");
    let shown = format!("{reason} {reason:?}");
    assert!(!shown.contains("SuperSecret99"));
    assert!(!shown.contains("mysql"));
}

#[test]
fn the_sensitivity_check_is_cheap_even_for_pathological_input() {
    let long = format!("echo {}", "'a ".repeat(1500));
    let started = std::time::Instant::now();
    let _ = is_sensitive(&long);
    let weird = "://".repeat(1500);
    let _ = is_sensitive(&weird);
    let at_signs = format!("x://{}", "@:".repeat(2000));
    let _ = is_sensitive(&at_signs);
    assert!(
        started.elapsed() < std::time::Duration::from_secs(2),
        "{:?}",
        started.elapsed()
    );
}
