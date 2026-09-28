-- MySQL dump 10.13  Distrib 8.4.11, for Linux (aarch64)
--
-- Host: localhost    Database: bindata
-- ------------------------------------------------------
-- Server version	8.4.11

/*!40101 SET @OLD_CHARACTER_SET_CLIENT=@@CHARACTER_SET_CLIENT */;
/*!40101 SET @OLD_CHARACTER_SET_RESULTS=@@CHARACTER_SET_RESULTS */;
/*!40101 SET @OLD_COLLATION_CONNECTION=@@COLLATION_CONNECTION */;
/*!50503 SET NAMES utf8mb4 */;
/*!40103 SET @OLD_TIME_ZONE=@@TIME_ZONE */;
/*!40103 SET TIME_ZONE='+00:00' */;
/*!40014 SET @OLD_UNIQUE_CHECKS=@@UNIQUE_CHECKS, UNIQUE_CHECKS=0 */;
/*!40014 SET @OLD_FOREIGN_KEY_CHECKS=@@FOREIGN_KEY_CHECKS, FOREIGN_KEY_CHECKS=0 */;
/*!40101 SET @OLD_SQL_MODE=@@SQL_MODE, SQL_MODE='NO_AUTO_VALUE_ON_ZERO' */;
/*!40111 SET @OLD_SQL_NOTES=@@SQL_NOTES, SQL_NOTES=0 */;

--
-- Current Database: `bindata`
--

CREATE DATABASE /*!32312 IF NOT EXISTS*/ `bindata` /*!40100 DEFAULT CHARACTER SET utf8mb4 COLLATE utf8mb4_0900_ai_ci */ /*!80016 DEFAULT ENCRYPTION='N' */;

USE `bindata`;

--
-- Table structure for table `blobs`
--

DROP TABLE IF EXISTS `blobs`;
/*!40101 SET @saved_cs_client     = @@character_set_client */;
/*!50503 SET character_set_client = utf8mb4 */;
CREATE TABLE `blobs` (
  `id` int NOT NULL,
  `b` blob,
  `tb` tinyblob,
  `mb` mediumblob,
  `lb` longblob,
  `vb` varbinary(300) DEFAULT NULL,
  `bn` binary(8) DEFAULT NULL,
  PRIMARY KEY (`id`),
  UNIQUE KEY `vb_unique` (`vb`)
) ENGINE=InnoDB DEFAULT CHARSET=utf8mb4 COLLATE=utf8mb4_0900_ai_ci;
/*!40101 SET character_set_client = @saved_cs_client */;

--
-- Dumping data for table `blobs`
--

LOCK TABLES `blobs` WRITE;
/*!40000 ALTER TABLE `blobs` DISABLE KEYS */;
INSERT INTO `blobs` VALUES (1,_binary '\0	\n\r\Z !\"#$%&\'()*+,-./0123456789:;<=>?@ABCDEFGHIJKLMNOPQRSTUVWXYZ[\\]^_`abcdefghijklmnopqrstuvwxyz{|}~€‚ƒ„…†‡ˆ‰Š‹Œ‘’“”•–—˜™š›œŸ ¡¢£¤¥¦§¨©ª«¬­®¯°±²³´µ¶·¸¹º»¼½¾¿ÀÁ\Â\Ã\Ä\Å\Æ\Ç\È\É\Ê\Ë\Ì\Í\Î\Ï\Ğ\Ñ\Ò\Ó\Ô\Õ\Ö\×\Ø\Ù\Ú\Û\Ü\İ\Ş\ß\à\á\â\ã\ä\å\æ\ç\è\é\ê\ë\ì\í\î\ï\ğ\ñ\ò\ó\ô\õ\ö\÷øùúûüışÿ',_binary 'ÿşıüûúùø\÷\ö\õ\ô\ó\ò\ñ\ğ\ï\î\í\ì\ë\ê\é\è\ç\æ\å\ä\ã\â\á\à\ß\Ş\İ\Ü\Û\Ú\Ù\Ø\×\Ö\Õ\Ô\Ó\Ò\Ñ\Ğ\Ï\Î\Í\Ì\Ë\Ê\É\È\Ç\Æ\Å\Ä\Ã\ÂÁÀ¿¾½¼»º¹¸·¶µ´³²±°¯®­¬«ª©¨§¦¥¤£¢¡ Ÿœ›š™˜—–•”“’‘Œ‹Š‰ˆ‡†…„ƒ‚€~}|{zyxwvutsrqponmlkjihgfedcba`_^]\\[ZYXWVUTSRQPONMLKJIHGFEDCBA@?>=<;:9876543210/.-,+*)(\'&%$#\"! \Z\r\n	',_binary '\0	\n\r\Z !\"#$%&\'()*+,-./0123456789:;<=>?@ABCDEFGHIJKLMNOPQRSTUVWXYZ[\\]^_`abcdefghijklmnopqrstuvwxyz{|}~€‚ƒ„…†‡ˆ‰Š‹Œ‘’“”•–—˜™š›œŸ ¡¢£¤¥¦§¨©ª«¬­®¯°±²³´µ¶·¸¹º»¼½¾¿ÀÁ\Â\Ã\Ä\Å\Æ\Ç\È\É\Ê\Ë\Ì\Í\Î\Ï\Ğ\Ñ\Ò\Ó\Ô\Õ\Ö\×\Ø\Ù\Ú\Û\Ü\İ\Ş\ß\à\á\â\ã\ä\å\æ\ç\è\é\ê\ë\ì\í\î\ï\ğ\ñ\ò\ó\ô\õ\ö\÷øùúûüışÿ',_binary '\0	\n\r\Z !\"#$%&\'()*+,-./0123456789:;<=>?@ABCDEFGHIJKLMNOPQRSTUVWXYZ[\\]^_`abcdefghijklmnopqrstuvwxyz{|}~€‚ƒ„…†‡ˆ‰Š‹Œ‘’“”•–—˜™š›œŸ ¡¢£¤¥¦§¨©ª«¬­®¯°±²³´µ¶·¸¹º»¼½¾¿ÀÁ\Â\Ã\Ä\Å\Æ\Ç\È\É\Ê\Ë\Ì\Í\Î\Ï\Ğ\Ñ\Ò\Ó\Ô\Õ\Ö\×\Ø\Ù\Ú\Û\Ü\İ\Ş\ß\à\á\â\ã\ä\å\æ\ç\è\é\ê\ë\ì\í\î\ï\ğ\ñ\ò\ó\ô\õ\ö\÷øùúûüışÿ',_binary '\0	\n\r\Z !\"#$%&\'()*+,-./0123456789:;<=>?@ABCDEFGHIJKLMNOPQRSTUVWXYZ[\\]^_`abcdefghijklmnopqrstuvwxyz{|}~€‚ƒ„…†‡ˆ‰Š‹Œ‘’“”•–—˜™š›œŸ ¡¢£¤¥¦§¨©ª«¬­®¯°±²³´µ¶·¸¹º»¼½¾¿ÀÁ\Â\Ã\Ä\Å\Æ\Ç\È\É\Ê\Ë\Ì\Í\Î\Ï\Ğ\Ñ\Ò\Ó\Ô\Õ\Ö\×\Ø\Ù\Ú\Û\Ü\İ\Ş\ß\à\á\â\ã\ä\å\æ\ç\è\é\ê\ë\ì\í\î\ï\ğ\ñ\ò\ó\ô\õ\ö\÷øùúûüışÿ',_binary '\0ÿ\'\"\\\n\r\Z'),(2,'','','','','',_binary '\0\0\0\0\0\0\0\0'),(3,NULL,NULL,NULL,NULL,NULL,NULL),(4,_binary 'hÃ©llo',_binary '\0',_binary 'text',_binary '\Ã',_binary 'ab  ',_binary 'ab\0\0\0\0\0\0'),(5,_binary 'ÿ',_binary '€',_binary '\â‚',_binary 'ğŸ˜€',_binary '\0',_binary '\0\0\0\0\0\0\0\0');
/*!40000 ALTER TABLE `blobs` ENABLE KEYS */;
UNLOCK TABLES;

--
-- Table structure for table `tokens`
--

DROP TABLE IF EXISTS `tokens`;
/*!40101 SET @saved_cs_client     = @@character_set_client */;
/*!50503 SET character_set_client = utf8mb4 */;
CREATE TABLE `tokens` (
  `id` binary(16) NOT NULL,
  `label` varchar(20) NOT NULL,
  PRIMARY KEY (`id`)
) ENGINE=InnoDB DEFAULT CHARSET=utf8mb4 COLLATE=utf8mb4_0900_ai_ci;
/*!40101 SET character_set_client = @saved_cs_client */;

--
-- Dumping data for table `tokens`
--

LOCK TABLES `tokens` WRITE;
/*!40000 ALTER TABLE `tokens` DISABLE KEYS */;
INSERT INTO `tokens` VALUES (_binary '\0	\n\r','counting'),(_binary '\'\\\n\r\Z\"\0\0\0\0\0\0\0\0\0\0','escapes'),(_binary 'ÿşıüûúùø\÷\ö\õ\ô\ó\ò\ñ\ğ','high');
/*!40000 ALTER TABLE `tokens` ENABLE KEYS */;
UNLOCK TABLES;

--
-- Dumping events for database 'bindata'
--

--
-- Dumping routines for database 'bindata'
--
/*!40103 SET TIME_ZONE=@OLD_TIME_ZONE */;

/*!40101 SET SQL_MODE=@OLD_SQL_MODE */;
/*!40014 SET FOREIGN_KEY_CHECKS=@OLD_FOREIGN_KEY_CHECKS */;
/*!40014 SET UNIQUE_CHECKS=@OLD_UNIQUE_CHECKS */;
/*!40101 SET CHARACTER_SET_CLIENT=@OLD_CHARACTER_SET_CLIENT */;
/*!40101 SET CHARACTER_SET_RESULTS=@OLD_CHARACTER_SET_RESULTS */;
/*!40101 SET COLLATION_CONNECTION=@OLD_COLLATION_CONNECTION */;
/*!40111 SET SQL_NOTES=@OLD_SQL_NOTES */;

-- Dump completed on 2026-09-28  8:07:20
