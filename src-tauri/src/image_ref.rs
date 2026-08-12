use crate::domain::ImageReference;
use crate::error::AppError;

pub fn parse_image_reference(
    input: &str,
    default_registry: &str,
) -> Result<ImageReference, AppError> {
    let input = input.trim();
    let default_registry = default_registry.trim();
    if input.is_empty() || !valid_registry(default_registry) {
        return invalid(input);
    }
    if input.contains(char::is_whitespace)
        || input.contains('\\')
        || input.starts_with('/')
        || input.ends_with('/')
        || input.contains("//")
    {
        return invalid(input);
    }

    let (registry, remainder) = split_registry(input, default_registry);
    if !valid_registry(registry) || remainder.is_empty() {
        return invalid(input);
    }

    let (repository, reference) = split_reference(remainder)
        .ok_or_else(|| AppError::InvalidImageReference(input.to_owned()))?;
    if !valid_repository(repository) {
        return invalid(input);
    }

    let repository = if repository.contains('/') {
        repository.to_owned()
    } else {
        format!("library/{repository}")
    };

    Ok(ImageReference {
        registry: registry.to_owned(),
        repository,
        reference: reference.to_owned(),
    })
}

fn split_registry<'a>(input: &'a str, default_registry: &'a str) -> (&'a str, &'a str) {
    match input.split_once('/') {
        Some((first, rest))
            if first == "localhost" || first.contains('.') || first.contains(':') =>
        {
            (first, rest)
        }
        _ => (default_registry, input),
    }
}

fn split_reference(value: &str) -> Option<(&str, &str)> {
    if value.matches('@').count() > 1 {
        return None;
    }
    if let Some((repository, digest)) = value.split_once('@') {
        if repository.contains(':') || !valid_digest(digest) {
            return None;
        }
        return Some((repository, digest));
    }

    match value.rsplit_once(':') {
        Some((repository, tag)) if !repository.is_empty() && !tag.is_empty() => {
            valid_tag(tag).then_some((repository, tag))
        }
        Some(_) => None,
        None => Some((value, "latest")),
    }
}

fn valid_digest(digest: &str) -> bool {
    let Some(payload) = digest.strip_prefix("sha256:") else {
        return false;
    };
    payload.len() == 64 && payload.bytes().all(|b| b.is_ascii_hexdigit())
}

fn valid_tag(tag: &str) -> bool {
    let bytes = tag.as_bytes();
    (1..=128).contains(&bytes.len())
        && (bytes[0].is_ascii_alphanumeric() || bytes[0] == b'_')
        && bytes[1..]
            .iter()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'_' | b'.' | b'-'))
}

fn valid_registry(registry: &str) -> bool {
    if registry.is_empty()
        || registry.contains('/')
        || registry.contains('@')
        || registry.chars().any(char::is_whitespace)
    {
        return false;
    }

    let (host, port) = match registry.rsplit_once(':') {
        Some((host, port)) => (host, Some(port)),
        None => (registry, None),
    };
    if host.contains(':')
        || port.is_some_and(|value| {
            value.is_empty()
                || !value.bytes().all(|byte| byte.is_ascii_digit())
                || !matches!(value.parse::<u16>(), Ok(1..=u16::MAX))
        })
    {
        return false;
    }

    if host == "localhost" {
        return true;
    }
    if host
        .bytes()
        .all(|byte| byte.is_ascii_digit() || byte == b'.')
        && host.contains('.')
    {
        return valid_ipv4(host);
    }
    valid_dns_hostname(host)
}

fn valid_ipv4(host: &str) -> bool {
    let octets: Vec<_> = host.split('.').collect();
    octets.len() == 4
        && octets.iter().all(|octet| {
            !octet.is_empty()
                && octet.bytes().all(|byte| byte.is_ascii_digit())
                && octet.parse::<u8>().is_ok()
        })
}

fn valid_dns_hostname(host: &str) -> bool {
    host.len() <= 253
        && host.split('.').all(|label| {
            !label.is_empty()
                && label.len() <= 63
                && label
                    .bytes()
                    .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-')
                && label
                    .as_bytes()
                    .first()
                    .is_some_and(|byte| byte.is_ascii_alphanumeric())
                && label
                    .as_bytes()
                    .last()
                    .is_some_and(|byte| byte.is_ascii_alphanumeric())
        })
}

fn valid_repository(repository: &str) -> bool {
    !repository.is_empty()
        && !repository.starts_with('/')
        && !repository.ends_with('/')
        && !repository.contains("//")
        && !repository.contains(':')
        && repository.split('/').all(valid_repository_component)
}

fn valid_repository_component(component: &str) -> bool {
    let bytes = component.as_bytes();
    if bytes.is_empty() || !is_lower_alphanumeric(bytes[0]) {
        return false;
    }

    let mut index = 1;
    while index < bytes.len() {
        if is_lower_alphanumeric(bytes[index]) {
            index += 1;
            continue;
        }

        let separator = bytes[index];
        if !matches!(separator, b'.' | b'_' | b'-') {
            return false;
        }

        let start = index;
        while index < bytes.len() && bytes[index] == separator {
            index += 1;
        }
        let count = index - start;
        let valid_count = match separator {
            b'.' => count == 1,
            b'_' => count <= 2,
            b'-' => count >= 1,
            _ => false,
        };
        if !valid_count || index == bytes.len() || !is_lower_alphanumeric(bytes[index]) {
            return false;
        }
    }
    true
}

fn is_lower_alphanumeric(byte: u8) -> bool {
    byte.is_ascii_lowercase() || byte.is_ascii_digit()
}

fn invalid<T>(input: &str) -> Result<T, AppError> {
    Err(AppError::InvalidImageReference(input.to_owned()))
}
