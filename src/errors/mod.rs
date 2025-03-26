use actix_web::{error, http::StatusCode, HttpResponse, Result};
use serde::{Deserialize, Serialize};

use std::fmt;
use std::fmt::{Display, Formatter};

#[derive(Debug, Deserialize, Serialize)]
pub struct ErrorResponse {
    pub message: String,
}

impl ErrorResponse {
    pub fn new<S: Into<String>>(error: S) -> Self {
        Self {
            message: error.into(),
        }
    }
}

#[derive(Debug, Serialize, Deserialize, PartialEq)]
pub enum AppError {
    BadRequest(String),
    Unauthorized(String),
    Forbidden(String),
    Conflict(String),
    DBError(String),
    HyperError(String),
    ActixError(String),
    NotFound(String),
    InternalServerError(String),
    QueryError(String),
    FromRowError(String),
}

impl AppError {
    pub fn new_error_message(&self) -> String {
        match self {
            AppError::BadRequest(msg) => {
                println!("Bad Request: {:?}", msg);
                msg.into()
            }

            AppError::Unauthorized(msg) => {
                println!("Unauthorised: {:?}", msg);
                msg.into()
            }

            AppError::Forbidden(msg) => {
                println!("Forbidden: {:?}", msg);
                msg.into()
            }

            AppError::Conflict(msg) => {
                println!("Conflict: {:?}", msg);
                msg.into()
            }

            AppError::NotFound(msg) => {
                println!("Not found error occurred: {:?}", msg);
                msg.into()
            }

            AppError::DBError(msg) => {
                println!("Database error: {:?}", msg);
                msg.into()
            }

            AppError::ActixError(msg) => {
                println!("Actix error: {:?}", msg);
                msg.into()
            }

            AppError::HyperError(msg) => {
                println!("Hyper error: {:?}", msg);
                msg.into()
            }

            AppError::InternalServerError(msg) => {
                println!("Internal Server error: {:?}", msg);
                msg.into()
            }

            AppError::QueryError(msg) => {
                println!("Internal Server error: {:?}", msg);
                msg.into()
            }

            AppError::FromRowError(msg) => {
                println!("Internal Server error: {:?}", msg);
                msg.into()
            }
        }
    }

    pub fn new_error_response(&self) -> ErrorResponse {
        ErrorResponse {
            message: self.to_string(),
        }
    }
}

/*
* Converts custom error types to (actix) http error code
*/
impl error::ResponseError for AppError {
    //---Status Code---
    fn status_code(&self) -> StatusCode {
        match self {
            AppError::BadRequest(_) => StatusCode::BAD_REQUEST,
            AppError::Unauthorized(_) => StatusCode::UNAUTHORIZED,
            AppError::Forbidden(_) => StatusCode::FORBIDDEN,
            AppError::NotFound(_) => StatusCode::NOT_FOUND,
            AppError::Conflict(_) => StatusCode::CONFLICT,
            AppError::InternalServerError(_) => StatusCode::INTERNAL_SERVER_ERROR,
            AppError::ActixError(_) => StatusCode::INTERNAL_SERVER_ERROR,
            AppError::DBError(_) => StatusCode::INTERNAL_SERVER_ERROR,
            AppError::HyperError(_) => StatusCode::INTERNAL_SERVER_ERROR,
            AppError::QueryError(_) => StatusCode::INTERNAL_SERVER_ERROR,
            AppError::FromRowError(_) => StatusCode::INTERNAL_SERVER_ERROR,
        }
    }

    //---Error response---
    fn error_response(&self) -> HttpResponse {
        HttpResponse::build(self.status_code()).json(ErrorResponse {
            message: self.new_error_message(),
        })
    }
}

impl Display for AppError {
    fn fmt(&self, f: &mut Formatter) -> Result<(), fmt::Error> {
        write!(f, "{}", self)
    }
}

/*
* Converts ActixError to custom AppError types
*/
impl From<actix_web::error::Error> for AppError {
    fn from(err: actix_web::error::Error) -> Self {
        AppError::ActixError(err.to_string())
    }
}


