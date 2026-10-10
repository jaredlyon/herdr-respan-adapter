use crate::setup::SetupCtx;
#[derive(Clone, Copy, Debug)]
pub enum Shell { Posix, Cmd, PowerShell }
pub fn argv(sc: &SetupCtx, kind: &str) -> Vec<String> {
    vec![sc.binary.to_string_lossy().into(),"hook".into(),kind.into(),"--state-dir".into(),sc.state_dir.to_string_lossy().into(),"--config-dir".into(),sc.config_dir.to_string_lossy().into()]
}
pub fn command(sc: &SetupCtx, kind: &str, shell: Shell) -> String {
    let args=argv(sc,kind);
    let quote=|text: &str| match shell {
        Shell::Posix => format!("'{}'",text.replace('\'',"'\\''")),
        Shell::PowerShell => format!("'{}'",text.replace('\'',"''")),
        Shell::Cmd => cmd_quote(text),
    };
    let text=args.iter().map(|s|quote(s)).collect::<Vec<_>>().join(" ");
    if matches!(shell,Shell::PowerShell) {format!("& {text}")} else {text}
}
fn cmd_quote(text:&str)->String {
    let mut result=String::from("\"");let mut slashes=0;
    for c in text.chars() {
        if c=='\\' {slashes+=1;continue;}
        if c=='%' {
            // CMD consumes ^ only outside quotes; native argv parsing rejoins the segments.
            result.extend(std::iter::repeat_n('\\',slashes*2));result.push_str("\"^%\"");
        } else if c=='"' {result.extend(std::iter::repeat_n('\\',slashes*2+1));result.push(c);}
        else {result.extend(std::iter::repeat_n('\\',slashes));result.push(c);}
        slashes=0;
    }
    result.extend(std::iter::repeat_n('\\',slashes*2));result.push('"');result
}

#[cfg(test)] mod tests {
    use super::*;
    #[test] fn quoted_paths_with_spaces() {
        let sc=SetupCtx {home:"/home/test".into(),state_dir:"/state with spaces".into(),config_dir:"/config with spaces".into(),project:None,binary:"/state with spaces/bin/herdr-respan".into()};
        let args=argv(&sc,"claude");assert_eq!(args[0],"/state with spaces/bin/herdr-respan");assert_eq!(args[1..3],["hook","claude"]);
        assert_eq!(command(&sc,"claude",Shell::Posix),"'/state with spaces/bin/herdr-respan' 'hook' 'claude' '--state-dir' '/state with spaces' '--config-dir' '/config with spaces'");
        assert_eq!(command(&sc,"claude",Shell::Cmd),"\"/state with spaces/bin/herdr-respan\" \"hook\" \"claude\" \"--state-dir\" \"/state with spaces\" \"--config-dir\" \"/config with spaces\"");
        assert_eq!(command(&sc,"claude",Shell::PowerShell),"& '/state with spaces/bin/herdr-respan' 'hook' 'claude' '--state-dir' '/state with spaces' '--config-dir' '/config with spaces'");
        assert_eq!(cmd_quote(r"C:\path with spaces\"),r#""C:\path with spaces\\""#);
        assert_eq!(cmd_quote("a%SystemRoot%b"),r#""a"^%"SystemRoot"^%"b""#);
        let mut sc=sc;sc.binary="/quote's/herdr-respan".into();assert!(command(&sc,"claude",Shell::Posix).contains("'\\''"));assert!(command(&sc,"claude",Shell::PowerShell).contains("quote''s"));
    }
}
