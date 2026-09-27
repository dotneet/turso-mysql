class User < ApplicationRecord
  has_many :posts
  has_many :tags, -> { distinct }, through: :posts

  validates :email, presence: true, uniqueness: true
end
