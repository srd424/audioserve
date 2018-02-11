use hyper::{self,Request, Response, Method, StatusCode};
use hyper::header::{ContentLength, ContentType, SetCookie, Cookie, Authorization, Bearer};
use futures::{Future,Stream, future};
use url::form_urlencoded;
use std::collections::HashMap;
use super::subs::short_response;
use ring::digest::{digest, SHA256};
use ring::hmac;
use ring::rand::{SystemRandom, SecureRandom};
use data_encoding::{BASE64};
use std::time::{SystemTime, UNIX_EPOCH};

pub trait Authenticator: Send+Sync {
    type Credentials;
    fn authenticate(&self, req: Request) -> Box<Future<Item=Result<(Request,Self::Credentials), Response>, Error=hyper::Error>>;
}

#[derive(Clone)]
pub struct SharedSecretAuthenticator {
    shared_secret: String,
    my_secret: Vec<u8>,
    token_validity_hours: u64

}

impl SharedSecretAuthenticator {
    pub fn new(shared_secret: String, my_secret: Vec<u8>, token_validity_hours: u64) -> Self {
        SharedSecretAuthenticator{
            shared_secret,
            my_secret,
            token_validity_hours
        }
    }
}

const COOKIE_NAME: &str = "audioserve_token";
const ACCESS_DENIED: &str = "Access denied";

type AuthResult = Result<(Request,()), Response>;
type AuthFuture = Box<Future<Item=AuthResult, Error=hyper::Error>>;
impl Authenticator for SharedSecretAuthenticator {
    type Credentials = ();
    fn authenticate(&self, req: Request) -> AuthFuture {
        fn deny() -> AuthResult {
            Err(short_response(StatusCode::Unauthorized, ACCESS_DENIED))
        }
        // this is part where client can authenticate itself and get token
        if req.method() == &Method::Post && req.path()=="/authenticate" {
            let auth = self.clone();
            return Box::new(req.body().concat2().map(move |b| {
                    
                let params = form_urlencoded::parse(b.as_ref()).into_owned()
                .collect::<HashMap<String, String>>();
                    
                if let Some(secret) = params.get("secret") {
                        debug!("Authenticating user");
                        if auth.auth_token_ok(secret) {
                            debug!("Authentication success");
                            let token = auth.new_auth_token();
                            Err(Response::new()
                                .with_header(ContentType::plaintext())
                                .with_header(ContentLength(token.len() as u64))
                                .with_header(SetCookie(vec![format!("{}={}; Max-Age={}",COOKIE_NAME, token,10*365*24*3600)]))
                                .with_body(token)
                            )
                        } else {
                           deny()
                        }
                        
                    } else {
                         deny()
                        
                    }
            
            }));
        };
        // And in this part we check token
        {
            let mut token = req.headers().get::<Authorization<Bearer>>().map(|h| h.0.token.as_str()) ;
            if token.is_none() {
                token = req.headers().get::<Cookie>().and_then(|h| h.get(COOKIE_NAME));
            }
            
            if token.is_none() || ! self.token_ok(token.unwrap()) {
                return Box::new(future::ok(deny()))
            } 
        }
        // If everything is ok we return credentials (in this case they are just unit type) and we return back request
        Box::new(future::ok(Ok((req,()))))
    }
}

impl SharedSecretAuthenticator {
    fn auth_token_ok(&self, token: &str) -> bool{
        let parts = token.split("|")
        .map(|s| BASE64.decode(s.as_bytes()))
        .filter_map(|r| r.ok())
        .collect::<Vec<_>>();
        if parts.len() == 2 {
            let mut hash2 = self.shared_secret.clone().into_bytes();
            let hash = &parts[1];
            hash2.extend(&parts[0]);
            let hash2 = digest(&SHA256, &hash2);

            return hash2.as_ref() == &hash[..]
        } else {
            error!("Incorrectly formed login token - {} parts", parts.len())
        }
        false
    }
    fn new_auth_token(&self) -> String {
        Token::new(self.token_validity_hours, &self.my_secret).into()
    }

    fn token_ok(&self, token: &str) -> bool {
        match token.parse::<Token>() {
        Ok(token) =>  {
            token.is_valid(&self.my_secret)
        }, 
        Err(e) => {
            warn!("Invalid token: {}", e);
            false
        }
        }
    }
}

#[derive(Clone,PartialEq,Debug)]
struct Token{
    random: [u8;32],
    validity: [u8;8],
    signature: [u8;32]
}

fn prepare_data(r: &[u8;32], v: &[u8;8]) -> [u8;40] {
    let mut to_sign = [0u8;40];
    &mut to_sign[0..32].copy_from_slice(&r[..]);
    &mut to_sign[32..40].copy_from_slice(&v[..]);
    to_sign
}

fn now() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).expect("Invalid system time").as_secs()
}

impl Token {

    fn new(token_validity_hours: u64, secret: &[u8]) -> Self {
        let mut random = [0u8;32];
        let rng = SystemRandom::new();
        rng.fill(&mut random).expect("Cannot generate random number");
        let validity: u64 = now()
            + token_validity_hours * 3600;
        let validity: [u8;8] = unsafe {::std::mem::transmute(validity.to_be())};
        let to_sign = prepare_data(&random, &validity);
        let key = hmac::SigningKey::new(&SHA256,secret);
        let sig = hmac::sign(&key, &to_sign);
        let slice = sig.as_ref();
        assert!(slice.len() == 32);
        let mut signature = [0u8;32];
        &mut signature.copy_from_slice(slice);
        
        Token{random, validity, signature}

    }

    fn is_valid(&self, secret: &[u8]) -> bool{
        let key = hmac::VerificationKey::new(&SHA256, secret);
        let data = prepare_data(&self.random, &self.validity);
        if  hmac::verify(&key, &data, &self.signature).is_err() {
            return false
        };
        

        return self.validity() > now();

    }

    fn validity(&self) -> u64 {
        let ts: u64 = unsafe {::std::mem::transmute_copy(&self.validity)};
        u64::from_be(ts)
    }
}

impl Into<String> for Token {
    fn into(self) -> String {
        let data = [&self.random[..], &self.validity[..], &self.signature[..]].concat();
        BASE64.encode(&data)
    }
}

quick_error! {
    #[derive(Debug, PartialEq)]
    enum TokenError {
        InvalidSize { }
        InvalidEncoding(error: ::data_encoding::DecodeError) {
            from()
        }
        
    }
}

impl ::std::str::FromStr for Token {
    type Err = TokenError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let bytes = BASE64.decode(s.as_bytes())?;
        if bytes.len() != 72 {
            return Err(TokenError::InvalidSize);
        };
        let mut random = [0u8; 32];
        let mut validity = [0u8;8];
        let mut signature = [0u8;32];

        &mut random.copy_from_slice(&bytes[0..32]);
        &mut validity.copy_from_slice(&bytes[32..40]);
        &mut signature.copy_from_slice(&bytes[40..72]);

        Ok(Token{random, validity, signature})
    }
}


#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_token() {
        let token = Token::new(24, b"my big secret");
        assert!(token.is_valid(b"my big secret"));
        let orig_token = token.clone();
        let serialized_token: String = token.into();
        assert!(serialized_token.len() >= 72);
        let new_token: Token = serialized_token.parse().unwrap();
        assert_eq!(orig_token, new_token);
        assert!(new_token.is_valid(b"my big secret"));
        assert!(! new_token.is_valid(b"wrong secret"));
        assert!(new_token.validity() -now() <= 24*3600);

    }
}